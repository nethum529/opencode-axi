//! Bounded HTTP identity and readiness probes for cold server admission.

use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const HEALTH_PATH: &str = "/global/health";
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(20);
const HEALTH_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(200);
/// A socket timeout of zero is rejected by the platform rather than applied, so
/// the last attempt of an exhausted budget keeps a floor that still reports the
/// real connection failure.
const MIN_HEALTH_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(1);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Identity returned by a healthy `OpenCode` server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerHealth {
    /// Version reported by the responding OpenCode process.
    pub version: String,
}

#[derive(Deserialize)]
struct HealthResponse {
    healthy: bool,
    version: String,
}

/// Probes the OpenCode-specific health endpoint once.
pub(crate) fn probe(port: u16, timeout: Duration) -> Result<ServerHealth, String> {
    let address = SocketAddr::new(LOOPBACK, port);
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| format!("health connection failed: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("could not bound health response read: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("could not bound health request write: {error}"))?;
    stream
        .write_all(
            format!(
                "GET {HEALTH_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .map_err(|error| format!("health request failed: {error}"))?;

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut response)
        .map_err(|error| format!("health response failed: {error}"))?;
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "health response exceeded {MAX_RESPONSE_BYTES} bytes"
        ));
    }
    parse_response(&response)
}

/// Polls the health endpoint until it succeeds or the caller's total budget expires.
///
/// Each network attempt consumes at most 200 ms. The caller supplies the total
/// cold-start budget (`start_timeout_ms`, 8,000 ms by default), so connection
/// refusal during process startup is retryable but can never hang admission.
pub(crate) fn wait_until_healthy<F>(budget: Duration, mut probe: F) -> Result<ServerHealth, String>
where
    F: FnMut(Duration) -> Result<ServerHealth, String>,
{
    let started = Instant::now();
    loop {
        let remaining = budget.saturating_sub(started.elapsed());
        let attempt_timeout = remaining.clamp(MIN_HEALTH_ATTEMPT_TIMEOUT, HEALTH_ATTEMPT_TIMEOUT);
        let error = match probe(attempt_timeout) {
            Ok(health) => return Ok(health),
            Err(error) => error,
        };

        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(error);
        }
        thread::sleep(HEALTH_POLL_INTERVAL.min(remaining));
    }
}

fn parse_response(response: &[u8]) -> Result<ServerHealth, String> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed
        .parse(response)
        .map_err(|error| format!("invalid health HTTP response: {error}"))?
    {
        httparse::Status::Complete(header_len) => header_len,
        httparse::Status::Partial => return Err("incomplete health HTTP response".to_owned()),
    };
    if parsed.code != Some(200) {
        return Err(format!(
            "health endpoint returned HTTP {}",
            parsed
                .code
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        ));
    }

    let body = if header_value(parsed.headers, "transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case(b"chunked"))
    {
        decode_chunked(&response[header_len..])?
    } else {
        response[header_len..].to_vec()
    };
    let health: HealthResponse = serde_json::from_slice(&body)
        .map_err(|error| format!("invalid OpenCode health response: {error}"))?;
    if !health.healthy {
        return Err("OpenCode health endpoint reported unhealthy".to_owned());
    }
    if health.version.trim().is_empty() {
        return Err("OpenCode health endpoint returned an empty version".to_owned());
    }
    Ok(ServerHealth {
        version: health.version,
    })
}

fn header_value<'a>(headers: &'a [httparse::Header<'a>], name: &str) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value)
}

fn decode_chunked(mut encoded: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::new();
    loop {
        let line_end = encoded
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| "invalid chunked health response".to_owned())?;
        let size_text = std::str::from_utf8(&encoded[..line_end])
            .map_err(|error| format!("invalid health chunk size: {error}"))?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default(), 16)
            .map_err(|error| format!("invalid health chunk size: {error}"))?;
        encoded = &encoded[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        if encoded.len() < size + 2 || &encoded[size..size + 2] != b"\r\n" {
            return Err("truncated chunked health response".to_owned());
        }
        decoded.extend_from_slice(&encoded[..size]);
        if decoded.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "health response exceeded {MAX_RESPONSE_BYTES} bytes"
            ));
        }
        encoded = &encoded[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::{ServerHealth, parse_response, probe, wait_until_healthy};
    use std::{
        cell::Cell,
        net::TcpListener,
        time::{Duration, Instant},
    };

    #[test]
    fn exhausted_budget_reports_the_real_refusal_not_a_zero_timeout() {
        let port = TcpListener::bind("127.0.0.1:0")
            .map(|listener| listener.local_addr().expect("bound address").port())
            .expect("a port nothing listens on");

        let started = Instant::now();
        let error = wait_until_healthy(Duration::from_millis(60), |attempt_timeout| {
            probe(port, attempt_timeout)
        })
        .expect_err("a closed port never becomes healthy");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "readiness against a closed port must stay within its budget"
        );
        assert!(
            !error.contains("0 duration"),
            "readiness must not surface its own timeout arithmetic: {error}"
        );
    }

    #[test]
    fn parses_open_code_health_identity() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n{\"healthy\":true,\"version\":\"1.18.10\"}";

        assert_eq!(
            parse_response(response).expect("valid OpenCode health response"),
            ServerHealth {
                version: "1.18.10".to_owned()
            }
        );
    }

    #[test]
    fn rejects_non_open_code_occupant_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";

        assert!(parse_response(response).is_err());
    }

    #[test]
    fn readiness_retries_connection_failure_within_bounded_budget() {
        let attempts = Cell::new(0);

        let health = wait_until_healthy(Duration::from_millis(100), |_| {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 3 {
                Err("connection refused".to_owned())
            } else {
                Ok(ServerHealth {
                    version: "1.18.10".to_owned(),
                })
            }
        })
        .expect("third readiness probe succeeds");

        assert_eq!(health.version, "1.18.10");
        assert_eq!(attempts.get(), 3);
    }
}
