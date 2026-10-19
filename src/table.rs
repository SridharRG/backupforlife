//! Plain-text tables and value formatting for terminal output.

use chrono::{DateTime, Local, Utc};

use crate::duration::format_duration;

pub fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut out = String::new();
        for (i, cell) in cells.enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            if i + 1 < widths.len() {
                out.extend(std::iter::repeat_n(
                    ' ',
                    widths[i].saturating_sub(cell.chars().count()),
                ));
            }
        }
        out.trim_end().to_string() + "\n"
    };
    let mut out = line(&mut headers.iter().copied());
    for row in rows {
        out.push_str(&line(&mut row.iter().map(String::as_str)));
    }
    out
}

pub fn print(headers: &[&str], rows: &[Vec<String>]) {
    print!("{}", render(headers, rows));
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `3h12m ago` / `in 2h` relative to `now`.
pub fn relative(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now - t;
    match delta.to_std() {
        Ok(past) if past.as_secs() < 60 => "just now".into(),
        Ok(past) => format!("{} ago", format_duration(round_minutes(past))),
        Err(_) => {
            let future = (t - now).to_std().unwrap_or_default();
            if future.as_secs() < 60 {
                "within a minute".into()
            } else {
                format!("in {}", format_duration(round_minutes(future)))
            }
        }
    }
}

fn round_minutes(d: std::time::Duration) -> std::time::Duration {
    std::time::Duration::from_secs(d.as_secs() / 60 * 60)
}

pub fn local_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn aligns_columns() {
        let out = render(
            &["A", "LONGER"],
            &[
                vec!["xxxx".into(), "1".into()],
                vec!["y".into(), "22".into()],
            ],
        );
        assert_eq!(out, "A     LONGER\nxxxx  1\ny     22\n");
    }

    #[test]
    fn bytes_and_relative_times() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(31_457_280), "30.0 MiB");
        assert_eq!(human_bytes(5 * 1024u64.pow(4) / 2), "2.5 TiB");
        let now = Utc::now();
        assert_eq!(relative(now - TimeDelta::seconds(30), now), "just now");
        assert_eq!(
            relative(now - TimeDelta::seconds(3 * 3600 + 12 * 60 + 5), now),
            "3h12m ago"
        );
        assert_eq!(relative(now + TimeDelta::hours(2), now), "in 2h");
    }
}
