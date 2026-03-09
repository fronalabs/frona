use axum::http::HeaderValue;
use cookie::Cookie;

pub fn make_refresh_cookie(token: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let c = Cookie::build(("refresh_token", token))
        .http_only(true)
        .same_site(cookie::SameSite::Lax)
        .path("/api/auth")
        .max_age(cookie::time::Duration::seconds(max_age_secs as i64))
        .secure(secure)
        .build();
    HeaderValue::from_str(&c.to_string()).expect("valid cookie header")
}

pub fn make_clear_refresh_cookie(secure: bool) -> HeaderValue {
    let c = Cookie::build(("refresh_token", ""))
        .http_only(true)
        .same_site(cookie::SameSite::Lax)
        .path("/api/auth")
        .max_age(cookie::time::Duration::ZERO)
        .secure(secure)
        .build();
    HeaderValue::from_str(&c.to_string()).expect("valid cookie header")
}

pub fn extract_refresh_token_from_cookie_header(header: &str) -> Option<&str> {
    extract_cookie_value(header, "refresh_token")
}

pub fn make_app_session_cookie(token: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let c = Cookie::build(("app_session", token))
        .http_only(true)
        .same_site(cookie::SameSite::Lax)
        .path("/apps/")
        .max_age(cookie::time::Duration::seconds(max_age_secs as i64))
        .secure(secure)
        .build();
    HeaderValue::from_str(&c.to_string()).expect("valid cookie header")
}

pub fn extract_app_session_from_cookie_header(header: &str) -> Option<&str> {
    extract_cookie_value(header, "app_session")
}

pub fn make_clear_app_session_cookie(secure: bool) -> HeaderValue {
    let c = Cookie::build(("app_session", ""))
        .http_only(true)
        .same_site(cookie::SameSite::Lax)
        .path("/apps/")
        .max_age(cookie::time::Duration::ZERO)
        .secure(secure)
        .build();
    HeaderValue::from_str(&c.to_string()).expect("valid cookie header")
}

fn extract_cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    Cookie::split_parse(header)
        .flatten()
        .find(|c| c.name() == name)
        .map(|c| {
            let value = c.value();
            let start = header.find(value)?;
            Some(&header[start..start + value.len()])
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_refresh_token_from_single_cookie() {
        let extracted = extract_refresh_token_from_cookie_header("refresh_token=abc123");
        assert_eq!(extracted, Some("abc123"));
    }

    #[test]
    fn extract_refresh_token_from_multiple_cookies() {
        let header = "other=value; refresh_token=mytoken; another=xyz";
        let extracted = extract_refresh_token_from_cookie_header(header);
        assert_eq!(extracted, Some("mytoken"));
    }

    #[test]
    fn extract_refresh_token_returns_none_when_absent() {
        let extracted = extract_refresh_token_from_cookie_header("other=value; session=abc");
        assert!(extracted.is_none());
    }

    #[test]
    fn extract_app_session_from_multiple_cookies() {
        let header = "refresh_token=abc; app_session=sess123; other=val";
        let extracted = extract_app_session_from_cookie_header(header);
        assert_eq!(extracted, Some("sess123"));
    }

    #[test]
    fn extract_app_session_returns_none_when_absent() {
        let extracted = extract_app_session_from_cookie_header("refresh_token=abc");
        assert!(extracted.is_none());
    }

    #[test]
    fn make_and_extract_refresh_cookie_round_trip() {
        let cookie = make_refresh_cookie("token-value", 3600, false);
        let header = cookie.to_str().unwrap();
        let extracted = extract_refresh_token_from_cookie_header(header);
        assert_eq!(extracted, Some("token-value"));
    }

    #[test]
    fn make_and_extract_app_session_round_trip() {
        let cookie = make_app_session_cookie("session-val", 1800, true);
        let header = cookie.to_str().unwrap();
        let extracted = extract_app_session_from_cookie_header(header);
        assert_eq!(extracted, Some("session-val"));
    }

    #[test]
    fn make_refresh_cookie_includes_secure_flag() {
        let cookie = make_refresh_cookie("tok", 60, true);
        let header = cookie.to_str().unwrap();
        assert!(header.contains("Secure"));
    }

    #[test]
    fn make_refresh_cookie_omits_secure_when_false() {
        let cookie = make_refresh_cookie("tok", 60, false);
        let header = cookie.to_str().unwrap();
        assert!(!header.contains("Secure"));
    }

    #[test]
    fn clear_refresh_cookie_has_zero_max_age() {
        let cookie = make_clear_refresh_cookie(false);
        let header = cookie.to_str().unwrap();
        assert!(header.contains("Max-Age=0"));
    }

    #[test]
    fn clear_app_session_cookie_has_zero_max_age() {
        let cookie = make_clear_app_session_cookie(false);
        let header = cookie.to_str().unwrap();
        assert!(header.contains("Max-Age=0"));
    }

    #[test]
    fn extract_from_empty_header_returns_none() {
        assert!(extract_refresh_token_from_cookie_header("").is_none());
        assert!(extract_app_session_from_cookie_header("").is_none());
    }
}
