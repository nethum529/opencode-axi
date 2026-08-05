//! Transport-stage and rate-limit metadata retained at the HTTP facade boundary.

use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::HeaderMap;

/// Whether a failed HTTP operation is known not to have sent request bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransmissionStage {
    BeforeTransmission,
    PossiblyTransmitted,
}

/// Retry metadata returned with an HTTP 429 response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RateLimit {
    retry_after_ms: Option<u64>,
}

impl RateLimit {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        Self::from_headers_at(headers, SystemTime::now())
    }

    fn from_headers_at(headers: &HeaderMap, now: SystemTime) -> Self {
        let now_ms = unix_millis(now);
        let retry_after =
            header(headers, "retry-after").and_then(|value| parse_retry_after(value, now_ms));
        let provider_reset = PROVIDER_RESET_HEADERS
            .iter()
            .filter_map(|name| header(headers, name))
            .filter_map(|value| parse_provider_reset(value, now_ms))
            .max();
        Self {
            retry_after_ms: retry_after.into_iter().chain(provider_reset).max(),
        }
    }

    #[must_use]
    pub const fn retry_after_ms(self) -> Option<u64> {
        self.retry_after_ms
    }
}

const PROVIDER_RESET_HEADERS: [&str; 7] = [
    "x-ratelimit-reset",
    "x-rate-limit-reset",
    "ratelimit-reset",
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
    "anthropic-ratelimit-requests-reset",
    "anthropic-ratelimit-tokens-reset",
];

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn parse_retry_after(value: &str, now_ms: u64) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .map(|seconds| seconds.saturating_mul(1_000))
        .or_else(|| parse_timestamp_ms(value).map(|reset| reset.saturating_sub(now_ms)))
}

fn parse_provider_reset(value: &str, now_ms: u64) -> Option<u64> {
    if let Ok(number) = value.parse::<u64>() {
        return Some(if number >= 1_000_000_000_000 {
            number.saturating_sub(now_ms)
        } else if number >= 1_000_000_000 {
            number.saturating_mul(1_000).saturating_sub(now_ms)
        } else {
            number.saturating_mul(1_000)
        });
    }
    parse_duration_ms(value)
        .or_else(|| parse_timestamp_ms(value).map(|reset| reset.saturating_sub(now_ms)))
}

fn parse_duration_ms(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut total = 0_u64;
    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if number_start == index {
            return None;
        }
        let number = value[number_start..index].parse::<u64>().ok()?;
        let multiplier = if value[index..].starts_with("ms") {
            index += 2;
            1
        } else if bytes.get(index) == Some(&b's') {
            index += 1;
            1_000
        } else if bytes.get(index) == Some(&b'm') {
            index += 1;
            60_000
        } else if bytes.get(index) == Some(&b'h') {
            index += 1;
            3_600_000
        } else {
            return None;
        };
        total = total.saturating_add(number.saturating_mul(multiplier));
    }
    Some(total)
}

fn parse_timestamp_ms(value: &str) -> Option<u64> {
    parse_rfc3339_ms(value).or_else(|| parse_http_date_ms(value))
}

fn parse_rfc3339_ms(value: &str) -> Option<u64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let (year, month, day) = parse_date(date)?;
    let (clock, fraction) = time
        .split_once('.')
        .map_or((time, None), |(clock, fraction)| (clock, Some(fraction)));
    let (hour, minute, second) = parse_clock(clock)?;
    let fraction_ms = match fraction {
        Some(digits) => {
            if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let mut padded = digits.as_bytes().iter().copied().chain([b'0'; 3]);
            let hundreds = u64::from(padded.next()? - b'0');
            let tens = u64::from(padded.next()? - b'0');
            let ones = u64::from(padded.next()? - b'0');
            hundreds * 100 + tens * 10 + ones
        }
        None => 0,
    };
    epoch_millis(year, month, day, hour, minute, second)?.checked_add(fraction_ms)
}

fn parse_http_date_ms(value: &str) -> Option<u64> {
    let fields = value.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() != 6 || fields[5] != "GMT" || !fields[0].ends_with(',') {
        return None;
    }
    let day = fields[1].parse().ok()?;
    let month = match fields[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = fields[3].parse().ok()?;
    let (hour, minute, second) = parse_clock(fields[4])?;
    epoch_millis(year, month, day, hour, minute, second)
}

fn parse_date(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((year, month, day))
}

fn parse_clock(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let hour = parts.next()?.parse().ok()?;
    let minute = parts.next()?.parse().ok()?;
    let second = parts.next()?.parse().ok()?;
    (parts.next().is_none() && hour < 24 && minute < 60 && second < 60)
        .then_some((hour, minute, second))
}

fn epoch_millis(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<u64> {
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))?;
    u64::try_from(seconds).ok()?.checked_mul(1_000)
}

const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn unix_millis(time: SystemTime) -> u64 {
    u64::try_from(
        time.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_and_provider_resets_are_normalized_to_milliseconds() {
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "2".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1700000003".parse().unwrap());
        assert_eq!(
            RateLimit::from_headers_at(&headers, now).retry_after_ms(),
            Some(3_000)
        );

        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "1m2s".parse().unwrap());
        assert_eq!(
            RateLimit::from_headers_at(&headers, now).retry_after_ms(),
            Some(62_000)
        );
    }

    #[test]
    fn retry_after_dates_and_rfc3339_provider_timestamps_are_supported() {
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_445_412_478);
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-requests-reset",
            "2015-10-21T07:28:01.250Z".parse().unwrap(),
        );
        assert_eq!(
            RateLimit::from_headers_at(&headers, now).retry_after_ms(),
            Some(3_250)
        );
    }
}
