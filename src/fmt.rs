//! Formateo de tamaños, antigüedades y rutas para mostrar en pantalla.

use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Tamaño en base 1024 con las etiquetas que usa el Explorador de Windows.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// «1 elemento», «2 elementos».
pub fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Separador de miles: «67.301».
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

/// Tiempo transcurrido como «0:21» o «1:02:05».
pub fn clock(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Bloque que va y viene: indica actividad cuando no se puede saber el avance.
pub fn pulse(width: usize, millis: u128) -> String {
    let block = (width / 5).max(1).min(width);
    let travel = width - block;
    let mut position = 0;
    if travel > 0 {
        let step = (millis / 60) as usize % (travel * 2);
        position = if step <= travel { step } else { travel * 2 - step };
    }
    format!(
        "{}{}{}",
        " ".repeat(position),
        "█".repeat(block),
        " ".repeat(travel - position)
    )
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Antigüedad legible de una fecha en segundos desde la época Unix.
pub fn age(modified: u64, now: u64) -> String {
    if modified == 0 {
        return "-".into();
    }
    let days = now.saturating_sub(modified) / 86_400;
    match days {
        0 => "hoy".into(),
        1..=30 => format!("{days} d"),
        31..=364 => format!("{} mes", days / 30),
        _ => format!("{} año", days / 365),
    }
}

/// Barra horizontal con resolución de octavos de celda.
pub fn bar(fraction: f64, width: usize) -> String {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let mut out = "█".repeat(full);
    if full < width {
        out.push(PARTIAL[eighths % 8]);
        out.push_str(&" ".repeat(width - full - 1));
    }
    out
}

/// Sustituye caracteres de control para que un nombre no pueda alterar la terminal.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Recorta por la izquierda: en una ruta lo que importa es el final.
pub fn fit_tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 1; // el «…» inicial
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        kept.push(c);
    }
    kept.push('…');
    kept.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_clock() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1.000");
        assert_eq!(thousands(67301), "67.301");
        assert_eq!(thousands(1234567), "1.234.567");
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(21), "0:21");
        assert_eq!(clock(3725), "1:02:05");
    }

    #[test]
    fn pulse_keeps_its_width_and_moves() {
        for millis in (0..20_000).step_by(37) {
            assert_eq!(pulse(40, millis).chars().count(), 40);
        }
        assert_ne!(pulse(40, 0), pulse(40, 300));
        assert_eq!(pulse(1, 500).chars().count(), 1);
        assert_eq!(pulse(0, 500), "");
    }

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KB");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn ages() {
        let now = 1_000_000_000;
        assert_eq!(age(0, now), "-");
        assert_eq!(age(now - 10, now), "hoy");
        assert_eq!(age(now - 3 * 86_400, now), "3 d");
        assert_eq!(age(now - 90 * 86_400, now), "3 mes");
        assert_eq!(age(now - 800 * 86_400, now), "2 año");
        assert_eq!(age(now + 500, now), "hoy");
    }

    #[test]
    fn bars_keep_their_width() {
        assert_eq!(bar(0.0, 4).chars().count(), 4);
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.5, 4), "██  ");
        assert_eq!(bar(2.0, 4), "████");
    }

    #[test]
    fn fit_tail_keeps_the_end() {
        assert_eq!(fit_tail("abcdef", 10), "abcdef");
        assert_eq!(fit_tail("abcdef", 4), "…def");
        assert_eq!(fit_tail("abcdef", 0), "");
        assert_eq!(fit_tail("日本語テキスト", 5), "…スト");
    }

    #[test]
    fn sanitize_removes_escape_sequences() {
        assert_eq!(sanitize("a\x1b[2Jb\n"), "a?[2Jb?");
    }
}
