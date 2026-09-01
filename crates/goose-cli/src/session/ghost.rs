//! La mascota: un fantasma en bloques con los ojos tallados por los espacios.
//!
//! Portado de ghostycode (`crates/tui/src/tui/underwater.rs`), donde vive
//! sobre un lienzo de ratatui. Aquí no hay lienzo —el REPL es línea a línea—,
//! así que el fantasma se anima en sitio con el cursor ANSI durante el arranque
//! y presta sus ojos al indicador de "pensando". Mismos glifos, misma cadencia,
//! misma línea de tiempo: la bienvenida de ghosty-lite y la de ghostycode son
//! el mismo personaje.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// Ancho de la mascota en celdas; el halo se dibuja dentro de este ancho.
pub const WIDTH: usize = 8;
pub const TOP: &str = " ▄████▄ ";
pub const BOTTOM: &str = "▐█▀██▀█▌";

/// Un frame cada ~180 ms: un parpadeo lento, nunca un spinner.
pub const FRAME_MS: u64 = 180;

/// Línea de tiempo de los ojos como tramos `(glifo, frames)`. Reposo mirando a
/// la izquierda; entre reposos, parpadeos, miradas, sonrisa, sorpresa, cariño,
/// una cabezada y un brillo. Todos los glifos ocupan una celda, así que la
/// mascota nunca se desplaza. Determinista: sin RNG.
const EYE_TIMELINE: &[(&str, u64)] = &[
    ("◐", 16),
    ("─", 2),
    ("◐", 14),
    ("◑", 5),
    ("◐", 12),
    ("─", 2),
    ("◐", 10),
    ("●", 5),
    ("◐", 12),
    ("◕", 6),
    ("◐", 10),
    ("─", 1),
    ("◐", 2),
    ("─", 1),
    ("◐", 12),
    ("◓", 4),
    ("◐", 12),
    ("◒", 4),
    ("◐", 14),
    ("◠", 7),
    ("◐", 12),
    ("◉", 3),
    ("●", 3),
    ("◐", 12),
    ("◔", 4),
    ("◑", 3),
    ("◐", 12),
    ("♥", 5),
    ("◐", 14),
    ("˘", 8),
    ("·", 6),
    ("˘", 8),
    ("─", 2),
    ("◐", 16),
    ("✧", 3),
    ("◕", 4),
    ("◐", 14),
];

/// Glifo de ojo para el frame `tick`.
pub fn eye(tick: u64) -> &'static str {
    let total: u64 = EYE_TIMELINE.iter().map(|(_, n)| *n).sum();
    let mut pos = tick % total.max(1);
    for (glyph, n) in EYE_TIMELINE {
        if pos < *n {
            return glyph;
        }
        pos -= *n;
    }
    "◐"
}

/// Fila de ojos para el frame `tick`. Mismo marco y ancho que las fijas.
pub fn eyes_row(tick: u64) -> String {
    let e = eye(tick);
    format!("▐ {e}  {e} ▌")
}

/// La chispa del halo lee el ánimo de los ojos y lo acompaña: `˚` en reposo,
/// `✦` con la sorpresa, `♡` con los corazones, `z` dormido, `✧` en el brillo.
fn halo_glyph(tick: u64) -> &'static str {
    match eye(tick) {
        "◉" => "✦",
        "♥" => "♡",
        "˘" | "·" => "z",
        "✧" => "✧",
        _ => "˚",
    }
}

/// Posición de la chispa: deriva de izquierda a derecha y vuelve, con un
/// periodo que no divide al de los ojos para que nunca vayan en fase.
fn halo_col(tick: u64) -> usize {
    const PATH: [usize; 10] = [2, 2, 3, 3, 4, 5, 5, 4, 3, 2];
    // 7 ticks por paso ≈ 1.3 s: una burbuja que sube, no un péndulo.
    PATH[((tick / 7) % PATH.len() as u64) as usize]
}

/// Halo para el frame `tick`: una partícula sobre la cabeza, siempre de
/// `WIDTH` celdas para que el bloque no se mueva.
pub fn halo_row(tick: u64) -> String {
    let col = halo_col(tick);
    let glyph = halo_glyph(tick);
    let mut row = String::with_capacity(WIDTH + 2);
    for _ in 0..col {
        row.push(' ');
    }
    row.push_str(glyph);
    for _ in (col + 1)..WIDTH {
        row.push(' ');
    }
    row
}

/// Las cuatro filas para el frame `tick`.
pub fn rows(tick: u64) -> [String; 4] {
    [
        halo_row(tick),
        TOP.to_string(),
        eyes_row(tick),
        BOTTOM.to_string(),
    ]
}

/// Frame actual según un reloj global arrancado en la primera consulta.
pub fn tick_now() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    (EPOCH.get_or_init(Instant::now).elapsed().as_millis() / u128::from(FRAME_MS)) as u64
}

/// Cuadros para un spinner (indicatif / cliclack): el fantasma parpadeando.
/// Sólo la fila de ojos, para que quepa delante de la frase.
pub fn spinner_frames() -> Vec<String> {
    // Un ciclo corto y expresivo, no la línea de tiempo entera (~300 frames).
    [
        "◐", "◐", "◐", "◐", "─", "◐", "◐", "◑", "◑", "◐", "◐", "●", "◐", "◐", "◕", "◐",
    ]
    .iter()
    .map(|e| format!("▐ {e}  {e} ▌"))
    .collect()
}

/// Pinta el fantasma con `info` a su derecha (una línea por fila, hasta 4) y,
/// si stdout es una terminal, lo deja parpadear en sitio durante `for_ms`
/// antes de devolver el control. Sin terminal imprime un solo cuadro.
///
/// Se dibuja con `\x1b[{n}A` (cursor arriba) y se reescriben las cuatro
/// filas: sin alt-screen, sin limpiar nada del scrollback.
pub fn print_animated(info: &[String], for_ms: u64, paint: impl Fn(&str) -> String) {
    let mut out = std::io::stdout().lock();
    let draw = |out: &mut std::io::StdoutLock<'_>, tick: u64| {
        for (i, row) in rows(tick).iter().enumerate() {
            let side = info.get(i).map(String::as_str).unwrap_or("");
            let _ = writeln!(out, "  {}   {}", paint(row), side);
        }
    };
    let _ = writeln!(out);
    let start = tick_now();
    draw(&mut out, start);
    let _ = out.flush();

    if !std::io::stdout().is_terminal() || for_ms == 0 {
        return;
    }
    let deadline = Instant::now() + Duration::from_millis(for_ms);
    let mut last = start;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(FRAME_MS / 3));
        let tick = tick_now();
        if tick == last {
            continue;
        }
        last = tick;
        let _ = write!(out, "\x1b[4A");
        draw(&mut out, tick);
        let _ = out.flush();
    }
    // Cierra en reposo, mirando a la izquierda: la pose canónica.
    let _ = write!(out, "\x1b[4A");
    draw(&mut out, 0);
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_eye_glyph_is_one_cell_wide() {
        let total: u64 = EYE_TIMELINE.iter().map(|(_, n)| *n).sum();
        let width = eyes_row(0).chars().count();
        for tick in 0..total {
            assert_eq!(eyes_row(tick).chars().count(), width, "tick {tick}");
        }
        assert_eq!(width, TOP.chars().count());
        assert_eq!(width, BOTTOM.chars().count());
        for tick in 0..total {
            assert_eq!(halo_row(tick).chars().count(), WIDTH, "halo tick {tick}");
        }
    }

    #[test]
    fn halo_follows_the_mood() {
        let total: u64 = EYE_TIMELINE.iter().map(|(_, n)| *n).sum();
        let seen: std::collections::BTreeSet<&str> = (0..total)
            .map(|t| halo_row(t).trim().to_string())
            .map(|s| Box::leak(s.into_boxed_str()) as &str)
            .collect();
        for g in ["˚", "✦", "♡", "z", "✧"] {
            assert!(seen.contains(g), "falta {g}");
        }
    }

    #[test]
    fn rest_pose_looks_left() {
        assert_eq!(eye(0), "◐");
    }
}
