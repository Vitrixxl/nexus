//! Nexus greeter — TUI de connexion pour greetd (lancé dans foot sous cage).
//! Sans $GREETD_SOCK : mode démo, mot de passe "nexus", Échap quitte.
//! `nexus-greeter --probe <user>` : vérifie le dialogue avec greetd (un essai volontairement faux).
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

type Rgb = [f32; 3];
const BG: Rgb = [0.020, 0.024, 0.039]; // #05060a
const DEEP: Rgb = [0.40, 0.125, 0.188]; // #662030
const ACCENT: Rgb = [1.0, 0.216, 0.373]; // #ff375f
const PURPLE: Rgb = [0.749, 0.353, 0.949]; // #bf5af2
const FG: Rgb = [0.957, 0.957, 0.961]; // #f4f4f5
const DIM: Rgb = [0.45, 0.45, 0.50];
const RED: Rgb = [1.0, 0.271, 0.227]; // #ff453a
const GREEN: Rgb = [0.561, 0.780, 0.604]; // #8fc79a

const FONT: [[&str; 5]; 5] = [
    ["#...#", "##..#", "#.#.#", "#..##", "#...#"],
    ["#####", "#....", "####.", "#....", "#####"],
    ["#...#", ".#.#.", "..#..", ".#.#.", "#...#"],
    ["#...#", "#...#", "#...#", "#...#", ".###."],
    [".####", "#....", ".###.", "....#", "####."],
];
const BW: usize = 34; // largeur de la boîte de saisie

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}
fn q(c: Rgb) -> [u8; 3] {
    c.map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8)
}
fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}
fn sh(args: &[&str]) -> String {
    Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Un aller-retour du protocole greetd : longueur u32 native puis JSON.
fn rpc(s: &mut UnixStream, msg: Value) -> std::io::Result<Value> {
    let body = msg.to_string();
    s.write_all(&(body.len() as u32).to_ne_bytes())?;
    s.write_all(body.as_bytes())?;
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let mut buf = vec![0; u32::from_ne_bytes(len) as usize];
    s.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

/// Authentifie auprès de greetd ; la connexion rendue sert ensuite à `start_session`.
fn login(sock: &str, user: &str, pass: &str) -> Result<UnixStream, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    let mut resp = rpc(&mut s, json!({"type": "create_session", "username": user}));
    for _ in 0..8 {
        let r = resp.map_err(|e| e.to_string())?;
        match r["type"].as_str() {
            Some("success") => return Ok(s),
            Some("auth_message") => {
                let answer = match r["auth_message_type"].as_str() {
                    Some("secret" | "visible") => json!(pass),
                    _ => Value::Null,
                };
                resp = rpc(
                    &mut s,
                    json!({"type": "post_auth_message_response", "response": answer}),
                );
            }
            _ => {
                let _ = rpc(&mut s, json!({"type": "cancel_session"}));
                return Err(match r["error_type"].as_str() {
                    Some("auth_error") => "mot de passe incorrect".into(),
                    _ => r["description"]
                        .as_str()
                        .unwrap_or("erreur greetd")
                        .to_string(),
                });
            }
        }
    }
    let _ = rpc(&mut s, json!({"type": "cancel_session"}));
    Err("trop de questions de PAM".into())
}

/// Sessions Wayland installées dont le binaire existe : (nom, commande).
fn sessions() -> Vec<(String, Vec<String>)> {
    let mut found = vec![];
    let mut files: Vec<_> = std::fs::read_dir("/usr/share/wayland-sessions")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    for text in files.iter().filter_map(|p| std::fs::read_to_string(p).ok()) {
        let key = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k))
                .map(str::to_string)
        };
        let (Some(name), Some(exec)) = (key("Name="), key("Exec=")) else {
            continue;
        };
        let cmd: Vec<String> = exec
            .split_whitespace()
            .filter(|a| !a.starts_with('%'))
            .map(String::from)
            .collect();
        let on_path = |b: &str| {
            std::env::var("PATH")
                .unwrap_or_default()
                .split(':')
                .any(|d| std::path::Path::new(d).join(b).exists())
        };
        if cmd.first().is_some_and(|b| {
            if b.contains('/') {
                std::path::Path::new(b).exists()
            } else {
                on_path(b)
            }
        }) {
            found.push((name, cmd));
        }
    }
    found
}

struct Rng(u32);
impl Rng {
    fn f(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 8) as f32 / 16_777_216.0
    }
}

/// Rend le terminal dans son état d'origine, même sur panic.
struct Tty(String);
impl Drop for Tty {
    fn drop(&mut self) {
        print!("\x1b[?2026l\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        sh(&["stty", &self.0]);
    }
}

/// Pixels en demi-blocs (2 par cellule en hauteur) + calque de texte.
struct Frame {
    c: usize,
    r: usize,
    px: Vec<Rgb>,
    text: Vec<Option<(char, Rgb, f32)>>,
    alpha: f32,
}
impl Frame {
    fn add(&mut self, x: f32, y: f32, col: Rgb) {
        let (xi, yi) = (x.round() as i64, y.round() as i64);
        if xi >= 0 && yi >= 0 && (xi as usize) < self.c && (yi as usize) < self.r * 2 {
            let p = &mut self.px[yi as usize * self.c + xi as usize];
            for i in 0..3 {
                p[i] += col[i];
            }
        }
    }
    fn put(&mut self, row: usize, col: i64, s: &str, color: Rgb) {
        for (i, ch) in s.chars().enumerate() {
            let x = col + i as i64;
            if row < self.r && x >= 0 && (x as usize) < self.c {
                self.text[row * self.c + x as usize] = Some((ch, color, self.alpha));
            }
        }
    }
    fn center(&mut self, row: usize, dx: i64, s: &str, color: Rgb) {
        self.put(
            row,
            (self.c as i64 - s.chars().count() as i64) / 2 + dx,
            s,
            color,
        );
    }
}

#[derive(PartialEq, Clone, Copy)]
enum State {
    Input,
    Auth,
    Fail,
    Exit,
}

struct Part {
    p: [f32; 2],
    v: [f32; 2],
    life: f32,
    max: f32,
    col: Rgb,
}

fn main() {
    let sock = std::env::var("GREETD_SOCK").ok();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--probe") {
        let (Some(sock), Some(user)) = (&sock, args.get(2)) else {
            return eprintln!("usage: GREETD_SOCK=… nexus-greeter --probe <user>");
        };
        return match login(sock, user, "\u{1}nexus-greeter-probe") {
            Err(e) if e == "mot de passe incorrect" => {
                println!("probe OK: greetd a demandé puis refusé le mot de passe")
            }
            other => println!("probe KO: {:?}", other.map(|_| "accepté ?!")),
        };
    }
    let saved = sh(&["stty", "-g"]);
    if saved.is_empty() {
        eprintln!("greeter: stdin n'est pas un terminal");
        return;
    }
    sh(&["stty", "raw", "-echo", "min", "0", "time", "0"]);
    let tty = Tty(saved);
    print!("\x1b[?1049h\x1b[?25l");

    let demo = sock.is_none();
    let mut users: Vec<String> = std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            let uid: u32 = f.get(2)?.parse().ok()?;
            ((1000..60000).contains(&uid) && !f.get(6)?.ends_with("nologin"))
                .then(|| f[0].to_string())
        })
        .collect();
    if demo || users.is_empty() {
        users = vec![std::env::var("USER").unwrap_or_else(|_| "user".into())];
    }
    let mut user_i = 0usize;
    let mut sess = sessions();
    if sess.is_empty() {
        sess.push(("Hyprland".into(), vec!["start-hyprland".into()]));
    }
    let (tx, rx) = mpsc::channel::<Result<Option<UnixStream>, String>>();
    let mut conn: Option<UnixStream> = None;
    let mut fail_msg = String::new();
    let host = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut rng = Rng(0x9e37_79b9);
    let start = Instant::now();
    let (mut c, mut r) = (0usize, 0usize);
    let (mut u, mut y0, mut top, mut lrows) = (2usize, 0usize, 0usize, 5usize);
    let mut logo: Vec<[f32; 5]> = vec![]; // cible x,y · départ x,y · délai
    let mut stars: Vec<[f32; 3]> = (0..180)
        .map(|_| [rng.f() * 2.0 - 1.0, rng.f() * 2.0 - 1.0, rng.f()])
        .collect();
    let mut parts: Vec<Part> = vec![];
    let (mut now, mut polled, mut last) = (String::new(), -1.0f32, 0.0f32);
    let (mut pass, mut session) = (String::new(), 0usize);
    let (mut state, mut since) = (State::Input, 0.0f32);
    let mut welcomed = false;
    let mut out = String::new();
    let mut stdin = std::io::stdin();

    loop {
        let frame_start = Instant::now();
        let t = start.elapsed().as_secs_f32();
        let dt = t - last;
        last = t;

        // ponytail: taille + heure via stty/date toutes les 0,5 s ; ioctl + localtime via libc si ça se voit un jour.
        if t - polled > 0.5 {
            polled = t;
            let size = sh(&["stty", "size"]);
            let mut it = size
                .split_whitespace()
                .filter_map(|v| v.parse::<usize>().ok());
            let (nr, nc) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
            let (nr, nc) = if nr < 2 || nc < 2 { (24, 80) } else { (nr, nc) };
            now = sh(&["date", "+%H:%M:%S|%A %d %B"]);
            if (nc, nr) != (c, r) {
                (c, r) = (nc, nr);
                u = ((c as f32 * 0.8 / 29.0) as usize)
                    .min((2 * r).saturating_sub(28) / 5)
                    .clamp(2, 8);
                lrows = (5 * u).div_ceil(2);
                top = r.saturating_sub(lrows + 12) / 2;
                let x0 = c.saturating_sub(29 * u) / 2;
                y0 = top * 2;
                logo.clear();
                for (li, glyph) in FONT.iter().enumerate() {
                    for (gy, line) in glyph.iter().enumerate() {
                        for (gx, _) in line.bytes().enumerate().filter(|(_, b)| *b == b'#') {
                            for d in 0..u * u {
                                logo.push([
                                    (x0 + (li * 6 + gx) * u + d % u) as f32,
                                    (y0 + gy * u + d / u) as f32,
                                    (rng.f() * 1.6 - 0.3) * c as f32,
                                    (rng.f() * 1.6 - 0.3) * 2.0 * r as f32,
                                    rng.f() * 0.4,
                                ]);
                            }
                        }
                    }
                }
            }
        }
        let (w, h) = (c as f32, 2.0 * r as f32);

        // ── clavier ──
        let mut buf = [0u8; 64];
        let n = stdin.read(&mut buf).unwrap_or(0);
        let inp = &buf[..n];
        if demo && (inp == [0x1b] || inp.contains(&3)) {
            break;
        }
        let user = users[user_i].clone();
        let mut burst = 0;
        if state == State::Input && inp.starts_with(&[0x1b]) {
            match inp {
                [0x1b] => pass.clear(),
                b"\x1b[A" => user_i = (user_i + users.len() - 1) % users.len(),
                b"\x1b[B" => user_i = (user_i + 1) % users.len(),
                _ => {}
            }
        } else if state == State::Input {
            for ch in String::from_utf8_lossy(inp).chars() {
                match ch {
                    '\r' | '\n' if !pass.is_empty() && state == State::Input => {
                        (state, since) = (State::Auth, t);
                        let (tx, sock, user, pass) =
                            (tx.clone(), sock.clone(), user.clone(), pass.clone());
                        std::thread::spawn(move || {
                            let _ = tx.send(match sock {
                                Some(sock) => login(&sock, &user, &pass).map(Some),
                                None => {
                                    std::thread::sleep(Duration::from_millis(900));
                                    if pass == "nexus" {
                                        Ok(None)
                                    } else {
                                        Err("mot de passe incorrect".into())
                                    }
                                }
                            });
                        });
                    }
                    '\x7f' | '\x08' => drop(pass.pop()),
                    '\x15' => pass.clear(),
                    '\t' => session = (session + 1) % sess.len(),
                    ch if !ch.is_control() && pass.chars().count() < 256 => {
                        pass.push(ch);
                        burst += 1;
                    }
                    _ => {}
                }
            }
        }
        if state == State::Auth
            && let Ok(res) = rx.try_recv()
        {
            since = t;
            match res {
                Ok(stream) => {
                    conn = stream;
                    (state, welcomed) = (State::Exit, true);
                    for _ in 0..350 {
                        let (a, s) = (rng.f() * std::f32::consts::TAU, 15.0 + rng.f() * 80.0);
                        parts.push(Part {
                            p: [w / 2.0, (top + lrows + 7) as f32 * 2.0],
                            v: [a.cos() * s, a.sin() * s],
                            life: 1.2,
                            max: 1.2,
                            col: mix(ACCENT, if rng.f() < 0.5 { PURPLE } else { FG }, rng.f()),
                        });
                    }
                }
                Err(msg) => {
                    (state, fail_msg) = (State::Fail, msg);
                    pass.clear();
                }
            }
        }
        match state {
            State::Fail if t - since > 1.3 => state = State::Input,
            State::Exit if t - since > 1.75 => break,
            _ => {}
        }
        let ts = t - since;
        let exit = state == State::Exit;
        let asm = if exit {
            (1.0 - ts).max(0.0)
        } else {
            (t / 1.8).min(1.0)
        }; // assemblage du logo
        let warp = if exit {
            0.15 + ts * ts * 5.0
        } else {
            0.15 + 3.5 * (-t * 2.0).exp()
        };
        let ui = if exit {
            (1.0 - ts * 3.0).max(0.0)
        } else {
            ((t - 1.5) / 0.6).clamp(0.0, 1.0)
        };

        let mut f = Frame {
            c,
            r,
            px: vec![BG; c * r * 2],
            text: vec![None; c * r],
            alpha: ui,
        };

        // ── fond : plasma + halo derrière le logo ──
        let (lcx, lcy) = (w / 2.0, y0 as f32 + 2.5 * u as f32);
        let diag = (w * w + h * h).sqrt() * 0.5;
        for y in 0..r * 2 {
            for x in 0..c {
                let (fx, fy) = (x as f32, y as f32);
                let (dx, dy) = (fx - w / 2.0, fy - h / 2.0);
                let d = (dx * dx + dy * dy).sqrt();
                let v = ((fx * 0.045 + t * 0.6).sin()
                    + (fy * 0.08 - t * 0.45).sin()
                    + ((fx + fy) * 0.03 + t * 0.3).sin()
                    + (d * 0.09 - t * 0.9).sin())
                    * 0.125
                    + 0.5;
                let hot = mix(ACCENT, PURPLE, 0.5 + 0.5 * (t * 0.2 + fx * 0.012).sin());
                let mut col = mix(BG, DEEP, (v * 1.3).min(1.0) * 0.7);
                let k = v.powi(4) * 0.55;
                let (gx, gy) = (
                    (fx - lcx) / (17.0 * u as f32),
                    (fy - lcy) / (6.0 * u as f32),
                );
                let glow = (-(gx * gx + gy * gy)).exp() * 0.22 * asm;
                let vig = (1.0 - (d / diag).powi(2)).max(0.0);
                for i in 0..3 {
                    col[i] = (col[i] + hot[i] * k) * vig + ACCENT[i] * glow;
                }
                f.px[y * c + x] = col;
            }
        }

        // ── étoiles (hyperespace à l'intro et à la sortie) ──
        for s in &mut stars {
            s[2] -= dt * warp * 0.5;
            if s[2] <= 0.02 {
                *s = [rng.f() * 2.0 - 1.0, rng.f() * 2.0 - 1.0, 1.0];
            }
            let b = (1.0 - s[2]).powi(2) * 0.8;
            for i in 0..(1.0 + warp * 3.0).min(14.0) as usize {
                let z = s[2] + i as f32 * 0.006 * warp;
                f.add(
                    w / 2.0 + s[0] / z * w / 2.0,
                    h / 2.0 + s[1] / z * h / 2.0,
                    FG.map(|v| v * b / (i + 1) as f32),
                );
            }
        }

        // ── logo : particules qui s'assemblent, reflet, glitch ──
        let glitch = asm >= 1.0 && t % 4.7 < 0.22;
        let sweep = ((t * 0.3).fract() * 1.8 - 0.4) * w;
        for &[tx, ty, sx, sy, delay] in &logo {
            let p = (asm * 1.4 - delay).clamp(0.0, 1.0);
            let e = 1.0 - (1.0 - p).powi(3);
            let (mut x, y) = (sx + (tx - sx) * e, sy + (ty - sy) * e);
            let band = (-((tx - ty * 0.6 - sweep) / (u as f32 * 2.0)).powi(2)).exp();
            let col = mix(
                mix(FG, ACCENT, (ty - y0 as f32) / (5 * u) as f32 * 0.9),
                [1.0; 3],
                band,
            )
            .map(|v| v * (0.3 + 0.7 * e));
            if glitch {
                let hsh = hash((ty as u32 / 2) ^ ((t * 18.0) as u32).wrapping_mul(2_654_435_761));
                if hsh & 3 == 0 {
                    x += (hsh >> 4) as f32 % 9.0 - 4.0;
                }
                f.add(x + 1.5, y, [col[0], 0.0, 0.0]);
                f.add(x - 1.5, y, [0.0, col[1], col[2]]);
            } else {
                f.add(x, y, col);
            }
        }

        // ── carte de connexion ──
        let ct = top + lrows + 2;
        let shake = if state == State::Fail {
            ((ts * 45.0).sin() * 3.0 * (1.0 - ts / 0.5).max(0.0)) as i64
        } else {
            0
        };
        let left = (c as i64 - BW as i64) / 2 + shake;
        let (time, date) = now.split_once('|').unwrap_or(("", ""));
        let spaced: String = time.chars().flat_map(|ch| [ch, ' ']).collect();
        f.center(ct, 0, spaced.trim_end(), FG);
        f.center(ct + 1, 0, date, DIM);
        for row in ct + 3..(ct + 8).min(r) {
            for x in left.max(0)..(left + BW as i64).min(c as i64) {
                for half in 0..2 {
                    let p = &mut f.px[(row * 2 + half) * c + x as usize];
                    *p = p.map(|v| v * (1.0 - 0.75 * ui));
                }
            }
        }
        let total = (2 * BW + 10) as f32;
        let border = |f: &mut Frame, row: usize, x: usize, ch: char, idx: usize| {
            let light = (std::f32::consts::PI * (idx as f32 / total - t * 0.25) * 2.0)
                .cos()
                .powi(8);
            let col = match state {
                State::Fail => RED,
                State::Exit => GREEN,
                State::Auth => mix(DEEP, ACCENT, 0.5 + 0.5 * (ts * 14.0).sin()),
                State::Input => mix([0.28, 0.28, 0.32], ACCENT, light),
            };
            f.put(row, left + x as i64, &ch.to_string(), col);
        };
        for x in 0..BW {
            let (a, m, b) = match x {
                0 => ('╭', '├', '╰'),
                x if x == BW - 1 => ('╮', '┤', '╯'),
                _ => ('─', '─', '─'),
            };
            border(&mut f, ct + 3, x, a, x);
            border(&mut f, ct + 5, x, m, x);
            border(&mut f, ct + 7, x, b, 2 * BW + 4 - x);
        }
        for j in 1..4 {
            border(&mut f, ct + 3 + j, BW - 1, '│', BW + j);
            border(&mut f, ct + 3 + j, 0, '│', 2 * BW + 9 - j);
        }
        f.put(ct + 4, left + 3, &user, FG);
        f.put(
            ct + 4,
            left + (BW - 3 - host.chars().count().min(BW - 3)) as i64,
            &host,
            DIM,
        );
        let len = pass.chars().count().min(BW - 7);
        if len == 0 && state == State::Input {
            f.put(ct + 6, left + 4, "mot de passe", [0.3, 0.3, 0.34]);
        }
        f.put(ct + 6, left + 3, &"●".repeat(len), ACCENT);
        if state == State::Input && (t * 2.0).fract() < 0.5 {
            f.put(ct + 6, left + 3 + len as i64, "▏", FG);
        }
        for _ in 0..burst * 12 {
            let (a, s) = (rng.f() * std::f32::consts::TAU, 6.0 + rng.f() * 22.0);
            parts.push(Part {
                p: [(left + 2 + len as i64) as f32, (ct + 6) as f32 * 2.0 + 0.5],
                v: [a.cos() * s, a.sin() * s - 6.0],
                life: 0.55,
                max: 0.55,
                col: mix(ACCENT, FG, rng.f() * 0.6),
            });
        }
        const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        match state {
            State::Input => f.center(ct + 8, 0, "entrée ↵ pour déverrouiller", [0.36, 0.36, 0.40]),
            State::Auth => f.center(
                ct + 8,
                0,
                &format!("{} authentification…", SPIN[(t * 12.0) as usize % 10]),
                ACCENT,
            ),
            State::Fail => f.center(ct + 8, shake, &fail_msg, RED),
            State::Exit => f.center(ct + 8, 0, &format!("bienvenue, {user}"), GREEN),
        }
        let name = sess[session].0.as_str();
        let sw = name.chars().count() as i64;
        f.center(ct + 10, 0, &format!("‹ {name} ›   tab"), DIM);
        f.put(ct + 10, (c as i64 - sw - 10) / 2 + 2, name, FG);
        let hint = match (demo, users.len() > 1) {
            (true, _) => "démo · mot de passe : nexus · échap pour quitter",
            (false, true) => "↑↓ utilisateur · tab session",
            (false, false) => "",
        };
        f.center(r - 1, 0, hint, [0.25, 0.25, 0.28]);

        // ── particules ──
        parts.retain_mut(|p| {
            p.p[0] += p.v[0] * dt;
            p.p[1] += p.v[1] * dt;
            p.v[1] += 30.0 * dt;
            p.life -= dt;
            p.life > 0.0
        });
        for p in &parts {
            f.add(p.p[0], p.p[1], p.col.map(|v| v * p.life / p.max));
        }

        // ── post : fondu d'entrée, voile rouge sur échec, flash blanc de sortie ──
        let fade_in = (t / 0.6).min(1.0);
        let red = if state == State::Fail {
            0.22 * (1.0 - ts / 0.4).max(0.0)
        } else {
            0.0
        };
        let flash = if exit {
            ((ts - 0.9) / 0.5).clamp(0.0, 1.0).powi(2)
        } else {
            0.0
        };
        let black = if exit {
            1.0 - ((ts - 1.45) / 0.3).clamp(0.0, 1.0)
        } else {
            1.0
        };
        for p in &mut f.px {
            *p = mix(mix(*p, RED, red), [1.0; 3], flash).map(|v| v * fade_in * black);
        }

        // ── sortie terminal ──
        out.clear();
        out.push_str("\x1b[?2026h\x1b[H");
        let (mut lf, mut lb) = (None, None);
        for row in 0..r {
            if row > 0 {
                out.push_str("\r\n");
            }
            for col in 0..c {
                let (tp, bt) = (f.px[row * 2 * c + col], f.px[(row * 2 + 1) * c + col]);
                let (fg, bg, ch) = match f.text[row * c + col] {
                    Some((ch, color, a)) => {
                        let under = mix(tp, bt, 0.5);
                        (q(mix(under, color, a)), q(under), ch)
                    }
                    None => (q(tp), q(bt), '▀'),
                };
                if lf != Some(fg) {
                    let _ = write!(out, "\x1b[38;2;{};{};{}m", fg[0], fg[1], fg[2]);
                    lf = Some(fg);
                }
                if lb != Some(bg) {
                    let _ = write!(out, "\x1b[48;2;{};{};{}m", bg[0], bg[1], bg[2]);
                    lb = Some(bg);
                }
                out.push(ch);
            }
        }
        out.push_str("\x1b[0m\x1b[?2026l");
        let mut so = std::io::stdout().lock();
        if so
            .write_all(out.as_bytes())
            .and_then(|_| so.flush())
            .is_err()
        {
            break;
        }
        drop(so);
        std::thread::sleep(Duration::from_millis(33).saturating_sub(frame_start.elapsed()));
    }

    drop(tty);
    if let Some(mut s) = conn {
        let env = [
            "XDG_SESSION_TYPE=wayland".to_string(),
            format!("XDG_SESSION_DESKTOP={}", sess[session].0),
        ];
        match rpc(
            &mut s,
            json!({"type": "start_session", "cmd": sess[session].1, "env": env}),
        ) {
            Ok(r) if r["type"] == "success" => {}
            other => eprintln!("greeter: start_session a échoué: {other:?}"),
        }
    } else if welcomed {
        println!("→ session {}", sess[session].0);
    }
}
