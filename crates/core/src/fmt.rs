//! Display formats for lists, the detail page and cache management.

use crate::browse::{Item, MediaSource};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};

pub const TICKS_PER_SEC: i64 = 10_000_000;

/// `x` with `digits` decimals, halves rounded up (as the old UI's `toFixed`; Rust's `{:.1}` rounds them to even).
fn fixed(x: f64, digits: usize) -> String {
    let m = 10_f64.powi(i32::try_from(digits).unwrap_or(0));
    format!("{:.*}", digits, (x * m).round() / m)
}

/// Ticks → `hh:mm:ss`, or `mm:ss` under an hour.
pub fn clock(ticks: i64) -> String {
    let s = ticks.max(0) / TICKS_PER_SEC;
    let (h, m, sec) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{sec:02}")
    } else {
        format!("{m:02}:{sec:02}")
    }
}

/// Runtime in minutes; empty when unknown or under half a minute, so the meta line skips it.
pub fn minutes(ticks: Option<i64>) -> String {
    let m = (ticks.unwrap_or(0) as f64 / TICKS_PER_SEC as f64 / 60.0).round();
    if m >= 1.0 {
        format!("{m}分钟")
    } else {
        String::new()
    }
}

/// An ISO date-time from the server as a calendar date at `offset`. A value without a zone is taken as already
/// local, a bare date as is.
fn day_at<Tz: TimeZone>(iso: &str, tz: &Tz) -> Option<NaiveDate> {
    if let Ok(t) = DateTime::parse_from_rfc3339(iso) {
        return Some(t.with_timezone(tz).date_naive());
    }
    if let Ok(t) = NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(t.date());
    }
    NaiveDate::parse_from_str(iso, "%Y-%m-%d").ok()
}

/// In local time on purpose: scrapers often store Beijing midnight as `T16:00:00Z`, so UTC would be a day early.
fn local_day(iso: &str) -> Option<NaiveDate> {
    day_at(iso, &Local)
}

/// Year; an ended series shows `2015 - 2025`.
pub fn years(it: &Item) -> String {
    let Some(start) = it.production_year.filter(|y| *y != 0) else {
        return String::new();
    };
    let end = it.end_date.as_deref().and_then(local_day).map(|d| d.year());
    match end {
        Some(end) if it.kind == "Series" && end != start => format!("{start} - {end}"),
        _ => start.to_string(),
    }
}

/// Community rating with one decimal; empty when missing or 0.
pub fn rating(r: Option<f64>) -> String {
    r.filter(|r| *r != 0.0)
        .map(|r| fixed(r, 1))
        .unwrap_or_default()
}

/// `2010年7月15日` in local time (see [`years`]); empty when missing or unreadable.
pub fn date(iso: Option<&str>) -> String {
    iso.and_then(local_day).map(cn_date).unwrap_or_default()
}

/// [`date`] at a fixed offset (tests, or a caller that knows the zone).
pub fn date_at(iso: Option<&str>, offset: FixedOffset) -> String {
    iso.and_then(|s| day_at(s, &offset))
        .map(cn_date)
        .unwrap_or_default()
}

fn cn_date(d: NaiveDate) -> String {
    format!("{}年{}月{}日", d.year(), d.month(), d.day())
}

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn ratio(n: u64, unit: u64) -> f64 {
    n as f64 / unit as f64
}

/// File size on the tech line: `4.85G`, `700M`.
fn size(bytes: Option<u64>) -> String {
    match bytes {
        None | Some(0) => String::new(),
        Some(b) if b >= GIB => format!("{}G", fixed(ratio(b, GIB), 2)),
        Some(b) => format!("{}M", ratio(b, MIB).round()),
    }
}

/// Cache usage: `0 KB`, `512 KB`, `446.3 MB`, `12.40 GB`.
pub fn bytes(n: u64) -> String {
    if n >= GIB {
        format!("{} GB", fixed(ratio(n, GIB), 2))
    } else if n >= MIB {
        format!("{} MB", fixed(ratio(n, MIB), 1))
    } else {
        format!("{} KB", ratio(n, 1024).round())
    }
}

/// Bit rate for the media info card: `12.3mbps`, `640kbps`.
pub fn bitrate(bps: Option<u64>) -> String {
    match bps {
        None | Some(0) => String::new(),
        Some(b) if b >= 1_000_000 => format!("{}mbps", fixed(ratio(b, 1_000_000), 1)),
        Some(b) => format!("{}kbps", ratio(b, 1000).round()),
    }
}

/// Episode subtitle `S01 · E02 · name`; other types show the year.
pub fn episode_label(it: &Item) -> String {
    if it.kind != "Episode" {
        return years(it);
    }
    let s = it.parent_index_number.map(|n| format!("S{n:02}"));
    let e = it.index_number.map(|n| format!("E{n:02}"));
    let name = Some(it.name.clone()).filter(|n| !n.is_empty());
    [s, e, name]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Tech line: `1080P SDR 23FPS 4.85G`; empty without a video track.
pub fn tech_line(src: Option<&MediaSource>) -> String {
    let Some(src) = src else {
        return String::new();
    };
    let Some(v) = src.media_streams.iter().find(|s| s.kind == "Video") else {
        return String::new();
    };
    // Scope sources are cropped in height (1920×800 is 1080P), 4:3 / HDV ones in width (1440×1080 is 1080P):
    // tier by each, keep the higher
    let (w, h) = (v.width.unwrap_or(0), v.height.unwrap_or(0));
    let by_w = match w {
        3800.. => 2160,
        2500.. => 1440,
        1900.. => 1080,
        1260.. => 720,
        _ => 0,
    };
    let by_h = match h {
        2000.. => 2160,
        1400.. => 1440,
        1000.. => 1080,
        700.. => 720,
        _ => h,
    };
    let res = match by_w.max(by_h) {
        2160.. => "4K".into(),
        1440.. => "2K".into(),
        0 => String::new(),
        t => format!("{t}P"),
    };
    let fps = v
        .real_frame_rate
        .or(v.average_frame_rate)
        .filter(|f| *f != 0.0)
        .map(|f| format!("{}FPS", f.round()));
    [Some(res), v.video_range.clone(), fps, Some(size(src.size))]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Series badge on a poster: unplayed count; when the server leaves it out (some only send it on certain
/// endpoints) the episode count `N集`, so series still stand out from movies. Nothing once played (the poster
/// shows a tick).
pub fn series_badge(it: &Item) -> String {
    if it.kind != "Series" || it.user_data.played {
        return String::new();
    }
    match (it.user_data.unplayed_item_count, it.recursive_item_count) {
        (Some(0), _) | (None, None | Some(0)) => String::new(),
        (Some(left), _) => left.to_string(),
        (None, Some(n)) => format!("{n}集"),
    }
}

/// A person's job on the detail page's cast row; unknown jobs as the server names them.
pub fn role(kind: &str) -> &str {
    match kind {
        "Actor" => "演员",
        "Director" => "导演",
        "Writer" => "编剧",
        "Producer" => "制片",
        "GuestStar" => "客串",
        "Composer" => "作曲",
        other => other,
    }
}

/// Label of the detail page's play button for `t`: resume point, episode, or plain play.
pub fn play_label(t: Option<&Item>) -> String {
    let pos = t.map_or(0, |t| t.user_data.playback_position_ticks);
    match t {
        _ if pos > 0 => format!("继续播放 {}", clock(pos)),
        Some(t) if t.kind == "Episode" => format!(
            "播放 S{} E{}",
            t.parent_index_number.unwrap_or(1),
            t.index_number.unwrap_or(1)
        ),
        _ => "播放".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browse::{MediaStream, UserData};

    const T: i64 = TICKS_PER_SEC;

    fn series() -> Item {
        Item {
            id: "1".into(),
            name: "x".into(),
            kind: "Series".into(),
            ..Default::default()
        }
    }

    #[test]
    fn minutes_round_and_skip_zero() {
        assert_eq!(minutes(Some(95 * 60 * T)), "95分钟", "whole");
        assert_eq!(minutes(Some(90 * T)), "2分钟", "rounds half up");
        assert_eq!(minutes(Some(29 * T)), "", "rounds to zero");
        assert_eq!(minutes(None), "", "unknown");
    }

    #[test]
    fn clock_under_and_over_an_hour() {
        assert_eq!(clock(0), "00:00", "zero");
        assert_eq!(clock(599_000_000), "00:59", "59.9 s floors");
        assert_eq!(clock(61 * T), "01:01", "minute");
        assert_eq!(clock(3600 * T), "01:00:00", "hour");
        assert_eq!(clock((2 * 3600 + 5 * 60 + 7) * T), "02:05:07", "hours");
    }

    /// Ended series as a range, everything else the start year.
    #[test]
    fn years_range_for_ended_series() {
        let it = |kind: &str, year: Option<i32>, end: Option<&str>| Item {
            kind: kind.into(),
            production_year: year,
            end_date: end.map(Into::into),
            ..series()
        };
        let end = Some("2025-03-01T00:00:00.0000000Z");
        assert_eq!(
            years(&it("Series", Some(2015), end)),
            "2015 - 2025",
            "ended"
        );
        assert_eq!(
            years(&it(
                "Series",
                Some(2015),
                Some("2015-12-01T00:00:00.0000000Z")
            )),
            "2015",
            "same year"
        );
        assert_eq!(years(&it("Series", Some(2015), None)), "2015", "running");
        assert_eq!(years(&it("Movie", Some(2015), end)), "2015", "not a series");
        assert_eq!(years(&it("Series", None, end)), "", "no start year");
    }

    /// Local midnight stored in UTC (T16:00Z at UTC+8) keeps its day.
    #[test]
    fn date_in_local_time() {
        let cst = FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(
            date_at(Some("2010-07-14T16:00:00.0000000Z"), cst),
            "2010年7月15日",
            "UTC+8"
        );
        assert_eq!(
            date_at(
                Some("2010-07-14T16:00:00.0000000Z"),
                FixedOffset::east_opt(0).unwrap()
            ),
            "2010年7月14日",
            "UTC"
        );
        assert_eq!(
            date_at(Some("2025-01-01"), cst),
            "2025年1月1日",
            "bare date"
        );
        assert_eq!(date(None), "", "missing");
        assert_eq!(date(Some("soon")), "", "unreadable");
        // Whatever this machine's zone: local midnight written as the server does keeps its day
        for (y, m, d) in [(2010, 7, 15), (2025, 1, 1)] {
            let midnight = Local.with_ymd_and_hms(y, m, d, 0, 0, 0).earliest().unwrap();
            let iso = midnight
                .to_utc()
                .format("%Y-%m-%dT%H:%M:%S.0000000Z")
                .to_string();
            assert_eq!(date(Some(&iso)), format!("{y}年{m}月{d}日"), "{iso}");
        }
    }

    #[test]
    fn rating_one_decimal() {
        assert_eq!(rating(Some(7.25)), "7.3", "halves up");
        assert_eq!(rating(Some(8.0)), "8.0", "keeps the decimal");
        assert_eq!(rating(Some(0.0)), "", "zero");
        assert_eq!(rating(None), "", "missing");
    }

    #[test]
    fn episode_label_or_year() {
        let ep = |s: Option<i32>, e: Option<i32>, name: &str| Item {
            kind: "Episode".into(),
            parent_index_number: s,
            index_number: e,
            name: name.into(),
            ..series()
        };
        assert_eq!(
            episode_label(&ep(Some(1), Some(2), "Pilot")),
            "S01 · E02 · Pilot",
            "full"
        );
        assert_eq!(
            episode_label(&ep(Some(0), Some(12), "SP")),
            "S00 · E12 · SP",
            "season 0"
        );
        assert_eq!(
            episode_label(&ep(None, None, "Special")),
            "Special",
            "name only"
        );
        let movie = Item {
            kind: "Movie".into(),
            production_year: Some(2001),
            ..series()
        };
        assert_eq!(episode_label(&movie), "2001", "a movie shows its year");
    }

    #[test]
    fn series_badge_cases() {
        let it = |left: Option<u32>, played: bool, count: Option<u32>| Item {
            user_data: UserData {
                unplayed_item_count: left,
                played,
                ..Default::default()
            },
            recursive_item_count: count,
            ..series()
        };
        assert_eq!(series_badge(&it(Some(3), false, Some(22))), "3", "unplayed");
        assert_eq!(
            series_badge(&it(Some(0), false, Some(22))),
            "",
            "all played"
        );
        assert_eq!(
            series_badge(&it(None, false, Some(22))),
            "22集",
            "count when unplayed is missing"
        );
        assert_eq!(series_badge(&it(None, true, Some(22))), "", "played");
        assert_eq!(series_badge(&it(None, false, None)), "", "nothing known");
        let movie = Item {
            kind: "Movie".into(),
            recursive_item_count: Some(5),
            ..series()
        };
        assert_eq!(series_badge(&movie), "", "not a series");
    }

    fn video(w: u32, h: u32) -> MediaStream {
        MediaStream {
            kind: "Video".into(),
            width: Some(w),
            height: Some(h),
            ..Default::default()
        }
    }

    fn source(size: Option<u64>, streams: Vec<MediaStream>) -> MediaSource {
        MediaSource {
            id: "s".into(),
            size,
            media_streams: streams,
            ..Default::default()
        }
    }

    #[test]
    fn tech_line_resolution_tiers() {
        let line = |w, h| tech_line(Some(&source(None, vec![video(w, h)])));
        assert_eq!(line(1920, 800), "1080P", "scope");
        assert_eq!(line(1280, 536), "720P", "scope 720");
        assert_eq!(line(3840, 1600), "4K", "scope 4K");
        assert_eq!(line(720, 480), "480P", "SD by height");
        assert_eq!(
            line(1440, 1080),
            "1080P",
            "HDV: the width alone only reaches 720P"
        );
        assert_eq!(line(960, 720), "720P", "4:3");
        assert_eq!(line(2560, 1440), "2K", "1440");
    }

    #[test]
    fn tech_line_range_rate_and_size() {
        let v = MediaStream {
            video_range: Some("HDR".into()),
            average_frame_rate: Some(23.976),
            ..video(1920, 1080)
        };
        let gib = 1_u64 << 30;
        assert_eq!(
            tech_line(Some(&source(Some(gib * 52 / 10), vec![v.clone()]))),
            "1080P HDR 24FPS 5.20G",
            "GB"
        );
        let real = MediaStream {
            real_frame_rate: Some(25.0),
            ..v
        };
        assert_eq!(
            tech_line(Some(&source(Some(700 << 20), vec![real]))),
            "1080P HDR 25FPS 700M",
            "MB, real rate first"
        );
        let audio = MediaStream {
            kind: "Audio".into(),
            index: 1,
            ..Default::default()
        };
        assert_eq!(
            tech_line(Some(&source(None, vec![audio]))),
            "",
            "no video track"
        );
        assert_eq!(tech_line(None), "", "no source");
    }

    #[test]
    fn cache_usage_and_bit_rate() {
        assert_eq!(bytes(0), "0 KB", "zero");
        assert_eq!(bytes(495_000), "483 KB", "KB");
        assert_eq!(bytes(467_982_131), "446.3 MB", "MB");
        assert_eq!(bytes(13_314_398_617), "12.40 GB", "GB");
        assert_eq!(bitrate(Some(12_345_678)), "12.3mbps", "mbps");
        assert_eq!(bitrate(Some(640_000)), "640kbps", "kbps");
        assert_eq!(bitrate(None), "", "missing");
    }

    #[test]
    fn roles_and_play_label() {
        assert_eq!(role("Director"), "导演", "known");
        assert_eq!(role("Lyricist"), "Lyricist", "unknown kept");
        assert_eq!(play_label(None), "播放", "nothing");
        let ep = Item {
            kind: "Episode".into(),
            parent_index_number: Some(2),
            index_number: Some(5),
            ..series()
        };
        assert_eq!(play_label(Some(&ep)), "播放 S2 E5", "episode");
        let resumed = Item {
            user_data: UserData {
                playback_position_ticks: 61 * T,
                ..Default::default()
            },
            ..ep
        };
        assert_eq!(play_label(Some(&resumed)), "继续播放 01:01", "resume point");
    }
}
