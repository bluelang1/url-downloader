//! S3 다운로드용 순수 로직: 자격 증명 파일 읽기, SigV4 presigned URL, MFA 세션(STS GetSessionToken), 세션 캐시, 입력 줄 해석.
//! 외부 CLI·AWS SDK 없이 `sha2`/`hmac` 과 기존 `reqwest`(blocking)·`serde_json` 만 쓴다.
//! 비밀 키·세션 토큰·MFA 코드는 어떤 에러 문자열·로그에도 넣지 않는다.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

/// presigned URL 최대 유효 시간 (S3 제한: 7일)
pub const MAX_PRESIGN_SECS: u64 = 604_800;
/// 캐시된 세션은 남은 시간이 이보다 짧으면 쓰지 않는다
pub const MIN_REMAINING_SECS: u64 = 60;
/// STS GetSessionToken 의 DurationSeconds 허용 범위 (15분 ~ 36시간)
pub const STS_MIN_DURATION: u64 = 900;
pub const STS_MAX_DURATION: u64 = 129_600;

const STS_ENDPOINT: &str = "https://sts.amazonaws.com";
const S3_GLOBAL_ENDPOINT: &str = "https://s3.amazonaws.com";
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

// ---------------------------------------------------------------- 자격 증명

#[derive(Clone, PartialEq, Eq)]
pub struct Creds {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    /// 만료 시각 (unix 초). 장기 키는 None
    pub expires_at: Option<u64>,
}

impl fmt::Debug for Creds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Creds")
            .field("access_key", &self.access_key)
            .field("secret_key", &"***")
            .field("session_token", &self.session_token.as_ref().map(|_| "***"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `~/.aws/credentials` 형식 텍스트에서 `[profile]` 섹션을 읽는다. 키 ID·비밀 키가 둘 다 있어야 Some.
pub fn parse_credentials(text: &str, profile: &str) -> Option<Creds> {
    let kv = ini_section(text, |name| name == profile)?;
    let get = |k: &str| ini_get(&kv, k);
    let access_key = get("aws_access_key_id")?;
    let secret_key = get("aws_secret_access_key")?;
    Some(Creds {
        access_key,
        secret_key,
        session_token: get("aws_session_token"),
        expires_at: get("aws_session_expiration").and_then(|v| parse_iso8601(&v)),
    })
}

/// `~/.aws/config` 의 `[default]`(profile 이 "default" 일 때) 또는 `[profile NAME]` 에서 `mfa_serial` 을 읽는다.
pub fn parse_mfa_serial(config_text: &str, profile: &str) -> Option<String> {
    let kv = ini_section(config_text, |name| {
        let parts: Vec<&str> = name.split_whitespace().collect();
        match parts[..] {
            ["profile", p] => p == profile,
            [p] => p == "default" && profile == "default",
            _ => false,
        }
    })?;
    ini_get(&kv, "mfa_serial")
}

/// 조건에 맞는 섹션들의 (소문자 키, 값) 목록. 섹션이 하나도 없으면 None.
fn ini_section(text: &str, want: impl Fn(&str) -> bool) -> Option<Vec<(String, String)>> {
    let (mut found, mut inside) = (false, false);
    let mut kv = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(h) = line.strip_prefix('[') {
            inside = h.find(']').is_some_and(|end| want(h[..end].trim()));
            found |= inside;
            continue;
        }
        if inside {
            if let Some((k, v)) = line.split_once('=') {
                kv.push((k.trim().to_ascii_lowercase(), strip_inline_comment(v).trim().to_string()));
            }
        }
    }
    found.then_some(kv)
}

/// 같은 키가 여러 번 나오면 마지막 값. 빈 값은 없는 것으로 본다.
fn ini_get(kv: &[(String, String)], key: &str) -> Option<String> {
    kv.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone()).filter(|v| !v.is_empty())
}

/// 공백 뒤에 오는 `#`/`;` 부터는 주석
fn strip_inline_comment(v: &str) -> &str {
    let mut prev_ws = false;
    for (i, c) in v.char_indices() {
        if (c == '#' || c == ';') && prev_ws {
            return &v[..i];
        }
        prev_ws = c.is_whitespace();
    }
    v
}

// ---------------------------------------------------------------- 날짜

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// unix 초 → `YYYYMMDD`
pub fn date_stamp(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    format!("{y:04}{m:02}{d:02}")
}

/// unix 초 → `YYYYMMDDTHHMMSSZ`
pub fn amz_date(unix: u64) -> String {
    let s = unix % 86_400;
    format!("{}T{:02}{:02}{:02}Z", date_stamp(unix), s / 3600, s / 60 % 60, s % 60)
}

/// ISO8601 (`2026-08-13T22:10:00Z`, `...+09:00`, `...+0900`, 소수 초 허용, 시간대 없으면 UTC) → unix 초
pub fn parse_iso8601(s: &str) -> Option<u64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> {
        let t = s.get(a..z)?;
        t.bytes().all(|c| c.is_ascii_digit()).then(|| t.parse().ok())?
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let mut rest = s.get(19..)?;
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &r[n..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits: String = rest[1..].chars().filter(|&c| c != ':').collect();
            if !digits.bytes().all(|c| c.is_ascii_digit()) || !matches!(digits.len(), 2 | 4) {
                return None;
            }
            let oh: i64 = digits[..2].parse().ok()?;
            let om: i64 = if digits.len() == 4 { digits[2..].parse().ok()? } else { 0 };
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    let t = days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + se - offset;
    u64::try_from(t).ok()
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 1970-01-01 기준 일수 (Howard Hinnant 의 days_from_civil)
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------- SigV4

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC 는 모든 키 길이를 받는다");
    m.update(data);
    m.finalize().into_bytes().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// RFC3986 unreserved(`A-Z a-z 0-9 - _ . ~`) 외 전부 `%XX`(대문자). `keep_slash` 면 `/` 는 그대로.
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 이미 인코딩된 (키, 값) 쌍을 키 순으로 정렬해 `k=v&...`
fn canonical_query(params: &[(String, String)]) -> String {
    let mut p = params.to_vec();
    p.sort();
    p.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// headers: (소문자 이름, 값), 이름 순 정렬돼 있어야 함
fn canonical_request(method: &str, uri: &str, query: &str, headers: &[(&str, &str)], payload_hash: &str) -> String {
    let canon_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed = signed_headers(headers);
    format!("{method}\n{uri}\n{query}\n{canon_headers}\n{signed}\n{payload_hash}")
}

fn signed_headers(headers: &[(&str, &str)]) -> String {
    headers.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";")
}

fn scope(unix: u64, region: &str, service: &str) -> String {
    format!("{}/{region}/{service}/aws4_request", date_stamp(unix))
}

fn signature(secret: &str, unix: u64, region: &str, service: &str, canonical: &str) -> String {
    let to_sign = format!("{ALGORITHM}\n{}\n{}\n{}", amz_date(unix), scope(unix, region, service), sha256_hex(canonical.as_bytes()));
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp(unix).as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    hex(&hmac(&k_signing, to_sign.as_bytes()))
}

/// S3 GET presigned URL. 버킷 이름에 `.` 이 없으면 가상 호스트형, 있으면(TLS 인증서 와일드카드가 안 맞으므로) 경로형.
/// `expires_secs` 는 1 ~ 604800(7일)로 맞춘다.
pub fn presign_get(creds: &Creds, region: &str, bucket: &str, key: &str, expires_secs: u64, now_unix: u64) -> String {
    let key = key.trim_start_matches('/');
    let (host, path) = if bucket.contains('.') {
        (format!("s3.{region}.amazonaws.com"), format!("/{bucket}/{key}"))
    } else {
        (format!("{bucket}.s3.{region}.amazonaws.com"), format!("/{key}"))
    };
    presign_get_with_host(creds, region, &host, &path, expires_secs, now_unix)
}

/// `path` 는 인코딩 전 경로(`/` 로 시작). `/` 는 보존하고 나머지는 퍼센트 인코딩해 서명한다.
pub fn presign_get_with_host(creds: &Creds, region: &str, host: &str, path: &str, expires_secs: u64, now_unix: u64) -> String {
    let uri = uri_encode(path, true);
    let credential = format!("{}/{}", creds.access_key, scope(now_unix, region, "s3"));
    let mut params = vec![
        ("X-Amz-Algorithm".to_string(), ALGORITHM.to_string()),
        ("X-Amz-Credential".to_string(), uri_encode(&credential, false)),
        ("X-Amz-Date".to_string(), amz_date(now_unix)),
        ("X-Amz-Expires".to_string(), expires_secs.clamp(1, MAX_PRESIGN_SECS).to_string()),
        ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
    ];
    if let Some(t) = &creds.session_token {
        params.push(("X-Amz-Security-Token".to_string(), uri_encode(t, false)));
    }
    let query = canonical_query(&params);
    let canonical = canonical_request("GET", &uri, &query, &[("host", host)], "UNSIGNED-PAYLOAD");
    let sig = signature(&creds.secret_key, now_unix, region, "s3", &canonical);
    format!("https://{host}{uri}?{query}&X-Amz-Signature={sig}")
}

// ---------------------------------------------------------------- 버킷 리전

/// `HEAD {endpoint}/{bucket}` 의 `x-amz-bucket-region` 헤더. 403/301 이어도 헤더가 오므로 상태 코드와 상관없이 본다.
/// 리다이렉트는 따라가지 않는다. `endpoint_override` 는 테스트용(기본 `https://s3.amazonaws.com`).
pub fn bucket_region(bucket: &str, endpoint_override: Option<&str>) -> Result<String, String> {
    if bucket.is_empty() || bucket.contains('/') {
        return Err(format!("버킷 이름이 올바르지 않음: {bucket}"));
    }
    let base = endpoint_override.unwrap_or(S3_GLOBAL_ENDPOINT).trim_end_matches('/');
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .head(format!("{base}/{}", uri_encode(bucket, false)))
        .send()
        .map_err(|e| format!("버킷 리전 확인 실패: {}", e.without_url()))?;
    let region = resp
        .headers()
        .get("x-amz-bucket-region")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    match region {
        Some(r) => Ok(r.to_string()),
        None if resp.status().as_u16() == 404 => Err(format!("버킷이 없음: {bucket}")),
        None => Err(format!("버킷 리전 확인 실패: HTTP {}", resp.status().as_u16())),
    }
}

// ---------------------------------------------------------------- STS (MFA 세션)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StsError {
    /// MFA 코드가 틀렸거나 형식이 맞지 않음 (AccessDenied 계열)
    BadCode,
    Other(String),
}

impl fmt::Display for StsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StsError::BadCode => write!(f, "MFA 코드가 올바르지 않음"),
            StsError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// 장기 키 + MFA 6자리 코드로 임시 세션을 받는다 (`POST https://sts.amazonaws.com/`, SigV4 헤더 서명).
/// `duration_secs` 는 900 ~ 129600 으로 맞춘다. 6자리 숫자가 아니면 요청 없이 `BadCode`.
pub fn get_session_token(
    long_term: &Creds,
    serial: &str,
    code: &str,
    duration_secs: u64,
    now_unix: u64,
    endpoint_override: Option<&str>,
) -> Result<Creds, StsError> {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|c| c.is_ascii_digit()) {
        return Err(StsError::BadCode);
    }
    let base = endpoint_override.unwrap_or(STS_ENDPOINT).trim_end_matches('/');
    let host = base.split_once("://").map_or(base, |(_, h)| h).split('/').next().unwrap_or("").to_string();
    let body = format!(
        "Action=GetSessionToken&Version=2011-06-15&DurationSeconds={}&SerialNumber={}&TokenCode={}",
        duration_secs.clamp(STS_MIN_DURATION, STS_MAX_DURATION),
        uri_encode(serial.trim(), false),
        code
    );
    let content_type = "application/x-www-form-urlencoded; charset=utf-8";
    let date = amz_date(now_unix);
    let mut headers = vec![("content-type", content_type), ("host", host.as_str()), ("x-amz-date", date.as_str())];
    if let Some(t) = &long_term.session_token {
        headers.push(("x-amz-security-token", t.as_str()));
    }
    let canonical = canonical_request("POST", "/", "", &headers, &sha256_hex(body.as_bytes()));
    let sig = signature(&long_term.secret_key, now_unix, "us-east-1", "sts", &canonical);
    let auth = format!(
        "{ALGORITHM} Credential={}/{}, SignedHeaders={}, Signature={sig}",
        long_term.access_key,
        scope(now_unix, "us-east-1", "sts"),
        signed_headers(&headers)
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| StsError::Other(e.to_string()))?;
    let mut req = client
        .post(format!("{base}/"))
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header("x-amz-date", &date)
        .header(reqwest::header::AUTHORIZATION, auth);
    if let Some(t) = &long_term.session_token {
        req = req.header("x-amz-security-token", t);
    }
    // 요청 본문(MFA 코드 포함)이 에러 문자열에 섞이지 않도록 reqwest 에러는 URL 없이만 쓴다
    let resp = req.body(body).send().map_err(|e| StsError::Other(format!("STS 요청 실패: {}", e.without_url())))?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(|e| StsError::Other(format!("STS 응답 읽기 실패: {}", e.without_url())))?;
    parse_sts_response(status, &text)
}

/// GetSessionToken 응답 XML 해석. 2xx 가 아니면 `<Code>`/`<Message>` 로 에러를 만든다.
fn parse_sts_response(status: u16, body: &str) -> Result<Creds, StsError> {
    if !(200..300).contains(&status) {
        let code = xml_tag(body, "Code").unwrap_or_default();
        if code == "AccessDenied" || code.starts_with("InvalidToken") || code == "InvalidAuthenticationCode" {
            return Err(StsError::BadCode);
        }
        let msg = xml_tag(body, "Message").unwrap_or_default();
        return Err(StsError::Other(match (code.is_empty(), msg.is_empty()) {
            (true, _) => format!("STS 오류: HTTP {status}"),
            (false, true) => format!("STS 오류: {code}"),
            (false, false) => format!("STS 오류: {code}: {msg}"),
        }));
    }
    let bad = || StsError::Other("STS 응답 형식이 올바르지 않음".to_string());
    let expiration = xml_tag(body, "Expiration").ok_or_else(bad)?;
    Ok(Creds {
        access_key: xml_tag(body, "AccessKeyId").ok_or_else(bad)?,
        secret_key: xml_tag(body, "SecretAccessKey").ok_or_else(bad)?,
        session_token: Some(xml_tag(body, "SessionToken").ok_or_else(bad)?),
        expires_at: Some(parse_iso8601(&expiration).ok_or_else(bad)?),
    })
}

/// `<tag>값</tag>` 의 첫 값 (앞뒤 공백 제거, 기본 XML 엔티티 해제). 비어 있으면 None.
fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    let v = xml[start..end]
        .trim()
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    (!v.is_empty()).then_some(v)
}

// ---------------------------------------------------------------- 세션 캐시

/// 세션을 JSON 으로 저장 (권한 0600, 같은 폴더 임시 파일에 쓴 뒤 rename)
pub fn save_session(path: &Path, creds: &Creds) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::json!({
        "access_key": creds.access_key,
        "secret_key": creds.secret_key,
        "session_token": creds.session_token,
        "expires_at": creds.expires_at,
    });
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    // 이미 있던 임시 파일이면 mode 가 적용되지 않으므로 한 번 더 맞춘다
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(json.to_string().as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// 저장된 세션. 파일이 없거나 깨졌거나, 토큰·만료 시각이 없거나, 남은 시간이 60초 미만이면 None.
pub fn load_session(path: &Path, now_unix: u64) -> Option<Creds> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
    let expires_at = v.get("expires_at")?.as_u64()?;
    if expires_at < now_unix.saturating_add(MIN_REMAINING_SECS) {
        return None;
    }
    Some(Creds {
        access_key: s("access_key")?,
        secret_key: s("secret_key")?,
        session_token: Some(s("session_token")?),
        expires_at: Some(expires_at),
    })
}

// ---------------------------------------------------------------- 입력 줄

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Line {
    /// 버킷 안 객체 키 (앞의 `/` 제거됨)
    pub key: String,
    /// 사용자가 준 저장 이름 (정리됨, 확장자 없음)
    pub name: Option<String>,
    /// `s3://버킷/키` 로 들어온 경우의 버킷
    pub bucket: Option<String>,
}

/// 한 줄 형식: `키` | `이름<Tab>키` | `이름 | 키` | `키<Tab>이름` | `키 | 이름` (`s3://버킷/키` 도 허용).
/// 구분자는 탭과 `|` 뿐 (공백·쉼표는 키의 일부). 두 조각 중 `s3://` 로 시작하는 쪽, 아니면 `/` 가 든 쪽이 키.
/// 판단이 안 서면(둘 다/둘 다 아님) 뒤쪽이 키. 빈 줄·`#` 주석·조각이 셋 이상이면 None.
pub fn parse_s3_line(line: &str) -> Option<S3Line> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = line.split(['\t', '|']).map(str::trim).filter(|p| !p.is_empty()).collect();
    let (key_raw, name_raw) = match parts[..] {
        [k] => (k, None),
        [a, b] => {
            let is_url = |p: &str| p.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("s3://"));
            let a_is_key = match (is_url(a), is_url(b)) {
                (true, false) => true,
                (false, true) => false,
                _ => a.contains('/') && !b.contains('/'),
            };
            if a_is_key { (a, Some(b)) } else { (b, Some(a)) }
        }
        _ => return None,
    };
    let (bucket, key) = match key_raw.get(..5).filter(|s| s.eq_ignore_ascii_case("s3://")) {
        Some(_) => {
            let (b, k) = key_raw[5..].split_once('/').unwrap_or((&key_raw[5..], ""));
            if b.is_empty() {
                return None;
            }
            (Some(b.to_string()), k)
        }
        None => (None, key_raw),
    };
    let key = key.trim_start_matches('/');
    if key.is_empty() {
        return None;
    }
    let name = name_raw.map(crate::sanitize).filter(|n| !n.is_empty());
    Some(S3Line { key: key.to_string(), name, bucket })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn example_creds() -> Creds {
        Creds {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            expires_at: None,
        }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ud_s3_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 요청 하나를 받아 원문(헤더+본문)을 채널로 넘기고 `response` 를 그대로 돌려주는 가짜 서버
    fn fake_server(response: String) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Some(Ok(mut s)) = listener.incoming().next() {
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let (mut raw, mut len) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let end = line.trim().is_empty();
                    raw.push_str(&line);
                    if end {
                        break;
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                raw.push_str(&String::from_utf8_lossy(&body));
                let _ = tx.send(raw);
                let _ = s.write_all(response.as_bytes());
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    // ---- 자격 증명 파일

    #[test]
    fn credentials_parse_profiles_comments_and_case() {
        let text = "\
# 맨 위 주석
[default]
aws_access_key_id = AKIDDEFAULT
aws_secret_access_key = secretdefault

; 세미콜론 주석
[ example-profile ]
  AWS_Access_Key_ID=AKIAIOSFODNN7EXAMPLE   # 줄 끝 주석
aws_secret_access_key   =   wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
Aws_Session_Token = FwoGZXIvYXdzEXAMPLETOKEN+/=
aws_session_expiration = 2026-08-13T22:10:00+00:00

[no-secret]
aws_access_key_id = AKIDONLY
";
        let c = parse_credentials(text, "example-profile").unwrap();
        assert_eq!(c.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(c.secret_key, "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        assert_eq!(c.session_token.as_deref(), Some("FwoGZXIvYXdzEXAMPLETOKEN+/="));
        assert_eq!(c.expires_at, parse_iso8601("2026-08-13T22:10:00Z"));
        assert!(c.expires_at.is_some());
        let d = parse_credentials(text, "default").unwrap();
        assert_eq!((d.access_key.as_str(), d.session_token, d.expires_at), ("AKIDDEFAULT", None, None));
        assert_eq!(parse_credentials(text, "no-secret"), None);
        assert_eq!(parse_credentials(text, "missing"), None);
        // Z 로 끝나는 만료 시각
        let z = parse_credentials("[p]\naws_access_key_id=a\naws_secret_access_key=b\naws_session_expiration=2026-08-13T22:10:00Z", "p");
        assert_eq!(z.unwrap().expires_at, parse_iso8601("2026-08-13T22:10:00+00:00"));
    }

    #[test]
    fn debug_hides_secrets() {
        let mut c = example_creds();
        c.session_token = Some("TOKENVALUE123".into());
        let s = format!("{c:?}");
        assert!(s.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!s.contains("EXAMPLEKEY") && !s.contains("TOKENVALUE123"), "{s}");
    }

    #[test]
    fn mfa_serial_from_config() {
        let cfg = "\
[default]
region = us-east-1
mfa_serial = arn:aws:iam::123456789012:mfa/default-user

[profile example-profile]
region = ap-northeast-2
MFA_Serial = arn:aws:iam::123456789012:mfa/example-user

[example-profile]
mfa_serial = should-not-match
";
        assert_eq!(parse_mfa_serial(cfg, "default").as_deref(), Some("arn:aws:iam::123456789012:mfa/default-user"));
        assert_eq!(parse_mfa_serial(cfg, "example-profile").as_deref(), Some("arn:aws:iam::123456789012:mfa/example-user"));
        assert_eq!(parse_mfa_serial("[profile   spaced ]\nmfa_serial=x", "spaced").as_deref(), Some("x"));
        assert_eq!(parse_mfa_serial(cfg, "other"), None);
        assert_eq!(parse_mfa_serial("[profile p]\nregion=x", "p"), None);
    }

    // ---- 날짜

    #[test]
    fn date_utils_with_leap_years() {
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(date_stamp(1_369_353_600), "20130524");
        assert_eq!(amz_date(0), "19700101T000000Z");
        // 2024-02-29 (4의 배수 윤년), 2000-02-29 (400의 배수 윤년)
        assert_eq!(parse_iso8601("2024-02-29T12:34:56Z"), Some(1_709_210_096));
        assert_eq!(amz_date(1_709_210_096), "20240229T123456Z");
        assert_eq!(parse_iso8601("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(amz_date(951_782_400 + 86_400), "20000301T000000Z");
        // 2100 은 윤년이 아니다
        assert_eq!(parse_iso8601("2100-02-29T00:00:00Z"), None);
        assert_eq!(amz_date(parse_iso8601("2100-02-28T23:59:59Z").unwrap() + 1), "21000301T000000Z");
        assert_eq!(parse_iso8601("2023-02-29T00:00:00Z"), None);
        // 연말 경계
        assert_eq!(amz_date(parse_iso8601("2025-12-31T23:59:59Z").unwrap() + 1), "20260101T000000Z");
        // 시간대·소수 초·시간대 없음
        let base = parse_iso8601("2026-08-13T13:10:00Z").unwrap();
        assert_eq!(parse_iso8601("2026-08-13T22:10:00+09:00"), Some(base));
        assert_eq!(parse_iso8601("2026-08-13T22:10:00+0900"), Some(base));
        assert_eq!(parse_iso8601("2026-08-13T08:10:00-05:00"), Some(base));
        assert_eq!(parse_iso8601("2026-08-13T13:10:00.611Z"), Some(base));
        assert_eq!(parse_iso8601("2026-08-13T13:10:00"), Some(base));
        assert_eq!(parse_iso8601("2011-07-11T19:55:29.611Z"), Some(1_310_414_129));
        for bad in ["", "2026-13-01T00:00:00Z", "2026-08-13 25:00:00Z", "2026/08/13T00:00:00Z", "2026-08-13T00:00:00X", "2026-08-13T00:00:00."] {
            assert_eq!(parse_iso8601(bad), None, "{bad}");
        }
    }

    // ---- SigV4

    #[test]
    fn presign_matches_aws_documentation_example() {
        let url = presign_get_with_host(&example_creds(), "us-east-1", "examplebucket.s3.amazonaws.com", "/test.txt", 86_400, 1_369_353_600);
        assert_eq!(
            url,
            "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    /// AWS SigV4 테스트 스위트 get-vanilla / post-x-www-form-urlencoded (헤더 서명 경로 검증)
    #[test]
    fn header_signing_matches_aws_test_suite() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let t = parse_iso8601("2015-08-30T12:36:00Z").unwrap();
        let empty = sha256_hex(b"");
        let get = canonical_request("GET", "/", "", &[("host", "example.amazonaws.com"), ("x-amz-date", "20150830T123600Z")], &empty);
        assert_eq!(signature(secret, t, "us-east-1", "service", &get), "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        let post = canonical_request(
            "POST",
            "/",
            "",
            &[("content-type", "application/x-www-form-urlencoded"), ("host", "example.amazonaws.com"), ("x-amz-date", "20150830T123600Z")],
            &sha256_hex(b"Param1=value1"),
        );
        assert_eq!(signature(secret, t, "us-east-1", "service", &post), "ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a");
    }

    #[test]
    fn presign_hosts_encoding_and_token() {
        let c = example_creds();
        let v = presign_get(&c, "ap-northeast-2", "example-bucket", "/my folder/한글 a+b,c~_.txt", 3600, 1_369_353_600);
        assert!(
            v.starts_with("https://example-bucket.s3.ap-northeast-2.amazonaws.com/my%20folder/%ED%95%9C%EA%B8%80%20a%2Bb%2Cc~_.txt?"),
            "{v}"
        );
        assert!(v.contains("X-Amz-Expires=3600&") && v.contains("%2Fap-northeast-2%2Fs3%2Faws4_request"));
        assert!(!v.contains("X-Amz-Security-Token"));
        let p = presign_get(&c, "eu-west-1", "example.bucket.dots", "a/b.txt", 3600, 1_369_353_600);
        assert!(p.starts_with("https://s3.eu-west-1.amazonaws.com/example.bucket.dots/a/b.txt?"), "{p}");
        // 7일 초과는 7일로
        assert!(presign_get(&c, "us-east-1", "example-bucket", "k", 10_000_000, 0).contains("X-Amz-Expires=604800&"));

        let mut s = c.clone();
        s.session_token = Some("FwoG/abc+def=".into());
        let t = presign_get(&s, "ap-northeast-2", "example-bucket", "my folder/한글 a+b,c~_.txt", 3600, 1_369_353_600);
        // 토큰은 서명 대상 쿼리에 포함되고(정렬 순서상 X-Amz-SignedHeaders 앞), 서명이 바뀐다
        assert!(t.contains("&X-Amz-Security-Token=FwoG%2Fabc%2Bdef%3D&X-Amz-SignedHeaders=host&X-Amz-Signature="), "{t}");
        let sig = |u: &str| u.rsplit("X-Amz-Signature=").next().unwrap().to_string();
        assert_ne!(sig(&v), sig(&t));
        assert_eq!(sig(&t).len(), 64);
    }

    // ---- 버킷 리전

    #[test]
    fn bucket_region_reads_header_without_following_redirect() {
        // 301 + Location 이 죽은 포트를 가리켜도 따라가지 않고 헤더만 읽는다
        let (base, rx) = fake_server(
            "HTTP/1.1 301 Moved Permanently\r\nx-amz-bucket-region: eu-west-1\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        );
        assert_eq!(bucket_region("example-bucket", Some(&base)), Ok("eu-west-1".to_string()));
        let req = rx.recv().unwrap();
        assert!(req.starts_with("HEAD /example-bucket HTTP/1.1\r\n"), "{req}");

        let (base, _rx) = fake_server("HTTP/1.1 403 Forbidden\r\nX-Amz-Bucket-Region: ap-northeast-2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into());
        assert_eq!(bucket_region("example-bucket", Some(&base)), Ok("ap-northeast-2".to_string()));

        let (base, _rx) = fake_server("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into());
        assert!(bucket_region("example-bucket", Some(&base)).unwrap_err().contains("example-bucket"));
        assert!(bucket_region("", None).is_err());
    }

    // ---- STS

    const STS_OK: &str = r#"<GetSessionTokenResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <GetSessionTokenResult>
    <Credentials>
      <SessionToken>
       AQoEXAMPLEH4aoAH0gNCAPyJxz4BlCFFxWNE1OPTgk5TthT+FvwqnKwRcOIfrRh3c/LTo6UDdyJwOOvEVPvLXCrrrUtdnniCEXAMPLE/IvU1dYUg2RVAJBanLiHb4IgRmpRV3zrkuWJOgQs8IZZaIv2BXIa2R4OlgkBN9bkUDNCJiBeb/AXlzBBko7b15fjrBs2+cTQtpZ3CYWFXG8C5zqx37wnOE49mRl/+OtkIKGO7fAE
      </SessionToken>
      <SecretAccessKey>
      wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY
      </SecretAccessKey>
      <Expiration>2011-07-11T19:55:29.611Z</Expiration>
      <AccessKeyId>ASIAIOSFODNN7EXAMPLE</AccessKeyId>
    </Credentials>
  </GetSessionTokenResult>
  <ResponseMetadata>
    <RequestId>58c5dbae-abef-11e0-8cfe-09039844ac7d</RequestId>
  </ResponseMetadata>
</GetSessionTokenResponse>"#;

    const STS_DENIED: &str = r#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <Error><Type>Sender</Type><Code>AccessDenied</Code><Message>MultiFactorAuthentication failed with invalid MFA one time pass code. </Message></Error>
  <RequestId>01234567-89ab-cdef-0123-456789abcdef</RequestId>
</ErrorResponse>"#;

    #[test]
    fn sts_response_parsing() {
        let c = parse_sts_response(200, STS_OK).unwrap();
        assert_eq!(c.access_key, "ASIAIOSFODNN7EXAMPLE");
        assert_eq!(c.secret_key, "wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY");
        assert!(c.session_token.as_deref().unwrap().starts_with("AQoEXAMPLEH4aoAH0gNCAPyJxz4BlCFFxWNE1OPTgk5TthT+"));
        assert!(c.session_token.as_deref().unwrap().ends_with("fAE"));
        assert_eq!(c.expires_at, Some(1_310_414_129));
        assert_eq!(parse_sts_response(403, STS_DENIED), Err(StsError::BadCode));
        let other = parse_sts_response(403, "<ErrorResponse><Error><Code>InvalidClientTokenId</Code><Message>bad key</Message></Error></ErrorResponse>");
        assert_eq!(other, Err(StsError::Other("STS 오류: InvalidClientTokenId: bad key".into())));
        assert!(matches!(parse_sts_response(500, "oops"), Err(StsError::Other(m)) if m.contains("500")));
        assert!(matches!(parse_sts_response(200, "<x/>"), Err(StsError::Other(_))));
    }

    #[test]
    fn sts_request_is_signed_and_parsed() {
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{STS_OK}", STS_OK.len());
        let (base, rx) = fake_server(resp);
        let serial = "arn:aws:iam::123456789012:mfa/example-user";
        let c = get_session_token(&example_creds(), serial, "123456", 43_200, 1_369_353_600, Some(&base)).unwrap();
        assert_eq!(c.access_key, "ASIAIOSFODNN7EXAMPLE");
        assert_eq!(c.expires_at, Some(1_310_414_129));
        let req = rx.recv().unwrap();
        assert!(req.starts_with("POST / HTTP/1.1\r\n"), "{req}");
        let lower = req.to_ascii_lowercase();
        let auth = req.lines().find(|l| l.to_ascii_lowercase().starts_with("authorization:")).unwrap();
        let prefix = "authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/sts/aws4_request, \
                      SignedHeaders=content-type;host;x-amz-date, Signature=";
        assert!(auth.to_string().to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase()), "{auth}");
        let sig = auth.rsplit("Signature=").next().unwrap().trim();
        assert!(sig.len() == 64 && sig.bytes().all(|b| b.is_ascii_hexdigit()), "{sig}");
        assert!(lower.contains("x-amz-date: 20130524t000000z\r\n"));
        assert!(lower.contains("content-type: application/x-www-form-urlencoded; charset=utf-8\r\n"));
        assert!(req.ends_with(
            "Action=GetSessionToken&Version=2011-06-15&DurationSeconds=43200\
             &SerialNumber=arn%3Aaws%3Aiam%3A%3A123456789012%3Amfa%2Fexample-user&TokenCode=123456"
        ));
    }

    #[test]
    fn sts_bad_code_paths() {
        // 형식이 틀린 코드는 요청 없이 BadCode
        for code in ["12345", "1234567", "12a456", ""] {
            assert_eq!(get_session_token(&example_creds(), "arn", code, 3600, 0, Some("http://127.0.0.1:1")), Err(StsError::BadCode));
        }
        // 서버가 AccessDenied → BadCode, 에러 문자열에 코드·비밀값이 섞이지 않음
        let resp = format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{STS_DENIED}", STS_DENIED.len());
        let (base, _rx) = fake_server(resp);
        let e = get_session_token(&example_creds(), "arn:aws:iam::123456789012:mfa/x", "654321", 3600, 0, Some(&base)).unwrap_err();
        assert_eq!(e, StsError::BadCode);
        // 연결 실패 에러에도 MFA 코드·비밀 키가 없다
        let e = get_session_token(&example_creds(), "arn", "654321", 3600, 0, Some("http://127.0.0.1:1")).unwrap_err();
        let msg = e.to_string();
        assert!(matches!(e, StsError::Other(_)));
        assert!(!msg.contains("654321") && !msg.contains("EXAMPLEKEY"), "{msg}");
    }

    // ---- 세션 캐시

    #[test]
    fn session_cache_roundtrip_permissions_and_expiry() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("session").join("sub").join("session.json");
        let creds = Creds {
            access_key: "ASIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY".into(),
            session_token: Some("AQoEXAMPLETOKEN".into()),
            expires_at: Some(10_000),
        };
        save_session(&path, &creds).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(load_session(&path, 5_000), Some(creds.clone()));
        assert_eq!(load_session(&path, 10_000 - 60), Some(creds.clone()));
        assert_eq!(load_session(&path, 10_000 - 59), None);
        assert_eq!(load_session(&path, 20_000), None);
        // 덮어쓰기
        let newer = Creds { expires_at: Some(50_000), ..creds.clone() };
        save_session(&path, &newer).unwrap();
        assert_eq!(load_session(&path, 20_000), Some(newer));
        // 깨진 파일·없는 파일·토큰 없는 세션
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_session(&path, 0), None);
        assert_eq!(load_session(&path.with_file_name("missing.json"), 0), None);
        save_session(&path, &Creds { session_token: None, ..creds }).unwrap();
        assert_eq!(load_session(&path, 0), None);
    }

    // ---- 입력 줄

    #[test]
    fn s3_line_forms() {
        let l = |key: &str, name: Option<&str>, bucket: Option<&str>| {
            Some(S3Line { key: key.into(), name: name.map(Into::into), bucket: bucket.map(Into::into) })
        };
        assert_eq!(parse_s3_line("reports/2024/a b,c.pdf"), l("reports/2024/a b,c.pdf", None, None));
        assert_eq!(parse_s3_line("/reports/a.pdf"), l("reports/a.pdf", None, None));
        assert_eq!(parse_s3_line("보고서\treports/a.pdf"), l("reports/a.pdf", Some("보고서"), None));
        assert_eq!(parse_s3_line("보고서 | reports/a.pdf"), l("reports/a.pdf", Some("보고서"), None));
        assert_eq!(parse_s3_line("reports/a.pdf\t보고서"), l("reports/a.pdf", Some("보고서"), None));
        assert_eq!(parse_s3_line("reports/a.pdf | 보고서"), l("reports/a.pdf", Some("보고서"), None));
        // 둘 다 `/` 없음, 둘 다 있음 → 뒤쪽이 키
        assert_eq!(parse_s3_line("이름 | a.pdf"), l("a.pdf", Some("이름"), None));
        assert_eq!(parse_s3_line("x/y | reports/a.pdf"), l("reports/a.pdf", Some("x_y"), None));
        // s3:// 접두사
        assert_eq!(parse_s3_line("s3://example-bucket/reports/a.pdf"), l("reports/a.pdf", None, Some("example-bucket")));
        assert_eq!(parse_s3_line("S3://example-bucket/a.pdf | 이름"), l("a.pdf", Some("이름"), Some("example-bucket")));
        assert_eq!(parse_s3_line("이름/슬래시 | s3://example-bucket/a.pdf"), l("a.pdf", Some("이름_슬래시"), Some("example-bucket")));
        assert_eq!(parse_s3_line("s3://example-bucket"), None);
        assert_eq!(parse_s3_line("s3:///a.pdf"), None);
        // 빈 이름 조각, 공백·쉼표는 구분자가 아님
        assert_eq!(parse_s3_line("  reports/a.pdf |  "), l("reports/a.pdf", None, None));
        assert_eq!(parse_s3_line("a b, c"), l("a b, c", None, None));
        // 빈 줄·주석·조각 셋 이상·키 없음
        for bad in ["", "   ", "# comment", "a | b | c", "/", " | "] {
            assert_eq!(parse_s3_line(bad), None, "{bad:?}");
        }
    }
}
