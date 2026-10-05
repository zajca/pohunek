//! Property tests of the Pi package's frame rules.
//!
//! A screen generator assembles transcript, upper border, draft, lower border
//! and footer from pieces of the screens captured from a real Pi
//! (`compat/pi/screens/`), mixed with hostile text, and checks each rule on its
//! own through the real manifest matcher: the working rule and the idle rule
//! never both match, the classification follows the upper border and nothing
//! else, and a busy upper border is never read as idle.

// Rust guideline compliant 2026-10-05

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, Instant};

use pohunek_daemon::detect::{
    DetectionConfig, Detector, DetectorConfig, Manifest, ManifestRegion, MatchContext,
};
use pohunek_test_support::workspace_root;
use protocol::{AgentActivity, StateSource};

/// Terminal height of the end-to-end checks.
const ROWS: u16 = 24;

/// Screens assembled per structural combination.
const SEEDS_PER_COMBINATION: u64 = 1;

/// Every n-th generated screen is also fed through a [`Detector`].
const END_TO_END_STRIDE: usize = 97;

fn detect_toml() -> String {
    fs::read_to_string(workspace_root().join("runtime-packages/pi/detect.toml"))
        .expect("read the Pi manifest")
}

/// The manifest with all rules, with only the working rule, and with only the
/// idle rule.
struct Manifests {
    full: Manifest,
    working: Manifest,
    idle: Manifest,
}

fn manifests() -> Manifests {
    let text = detect_toml();
    let parts: Vec<&str> = text.split("[[rules]]").collect();
    assert_eq!(parts.len(), 3, "the manifest declares the two frame rules");
    let only = |rule: &str| {
        Manifest::parse_str(&format!("{}[[rules]]{rule}", parts[0])).expect("single-rule manifest")
    };
    Manifests {
        full: Manifest::parse_str(&text).expect("the manifest parses"),
        working: only(parts[1]),
        idle: only(parts[2]),
    }
}

/// What `manifest` reads from screen `lines`, as the whole-screen region the
/// detector builds (trailing empty rows dropped).
fn read(manifest: &Manifest, lines: &[String]) -> Option<AgentActivity> {
    let mut lines = lines.to_vec();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let context =
        MatchContext::default().with_region_text(ManifestRegion::WholeRecent, lines.join("\n"));
    manifest
        .match_context(&context)
        .map(|matched| matched.activity)
}

const RULE: char = '\u{2500}';

fn is_braille(ch: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&ch)
}

/// Whether `line` is a border, by the definition in `detect.toml` and written
/// independently of its regular expressions.
fn is_border(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let leading = chars.iter().take_while(|ch| **ch == RULE).count();
    if leading == 2 && chars.get(2) == Some(&' ') && chars.get(3).is_some_and(|ch| is_braille(*ch))
    {
        return true;
    }
    if leading == chars.len() {
        return leading >= 20;
    }
    let trailing = chars.iter().rev().take_while(|ch| **ch == RULE).count();
    leading >= 4
        && trailing >= 4
        && chars.get(leading) == Some(&' ')
        && matches!(chars.get(leading + 1), Some('\u{2191}' | '\u{2193}'))
        && chars.get(leading + 2) == Some(&' ')
        && leading + 3 <= chars.len() - trailing
}

fn is_busy_border(line: &str) -> bool {
    line.starts_with("\u{2500}\u{2500} ") && line.chars().nth(3).is_some_and(is_braille)
}

/// Pieces of the captured screens, by role.
#[derive(Default)]
struct Pieces {
    /// Upper borders (busy and plain) by terminal width.
    upper: BTreeMap<usize, Vec<String>>,
    /// Lower borders by terminal width.
    lower: BTreeMap<usize, Vec<String>>,
    transcript: Vec<String>,
    draft: Vec<String>,
    footer: Vec<String>,
}

fn push_unique(list: &mut Vec<String>, line: &str) {
    if !list.iter().any(|existing| existing == line) {
        list.push(line.to_owned());
    }
}

fn pieces() -> Pieces {
    let root = workspace_root().join("compat/pi/screens");
    let mut files = Vec::new();
    for dir in [root.clone(), root.join("widths")] {
        for entry in fs::read_dir(&dir).expect("read the screens directory") {
            let path = entry.expect("directory entry").path();
            if path.extension().is_some_and(|extension| extension == "txt") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(files.len() >= 108, "the captured screens: {}", files.len());
    let mut pieces = Pieces::default();
    for path in files {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("UTF-8 file name")
            .to_owned();
        let width = stem
            .rsplit_once("_w")
            .map_or(100, |(_, width)| width.parse().expect("a column count"));
        let text = fs::read_to_string(&path).expect("read the screen");
        let rows: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
        let borders: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.starts_with("\u{2500}\u{2500}"))
            .map(|(index, _)| index)
            .collect();
        let (upper, lower) = (borders[borders.len() - 2], borders[borders.len() - 1]);
        push_unique(pieces.upper.entry(width).or_default(), rows[upper]);
        push_unique(pieces.lower.entry(width).or_default(), rows[lower]);
        for row in &rows[..upper] {
            push_unique(&mut pieces.transcript, row);
        }
        for row in &rows[upper + 1..lower] {
            assert!(!is_border(row), "{stem}: a draft row is a border: {row:?}");
            push_unique(&mut pieces.draft, row);
        }
        for row in &rows[lower + 1..] {
            assert!(!is_border(row), "{stem}: a footer row is a border: {row:?}");
            push_unique(&mut pieces.footer, row);
        }
    }
    pieces
}

/// Text that is not a border, however much it looks like one.
fn hostile_text() -> Vec<String> {
    let rule = |count: usize| RULE.to_string().repeat(count);
    vec![
        "\u{2500} example".to_owned(),
        "\u{2500}\u{2500} example".to_owned(),
        format!("{} example", rule(12)),
        rule(1),
        rule(2),
        rule(11),
        rule(19),
        format!("{} \u{2191} 5 more {}", rule(4), rule(2)),
        format!("{} \u{2191} 5 more", rule(4)),
        "\u{2807} Working".to_owned(),
        "Working".to_owned(),
        "Retrying (1/3) in 1s...".to_owned(),
        "\u{2191} 5 more".to_owned(),
        format!("x {} \u{2191} 5 more", rule(12)),
        format!("{} \u{2807} Working", rule(12)),
        "\u{2500}\u{2500}\u{2500} \u{2807} Working".to_owned(),
    ]
}

/// Lines that are borders, or start like one. Above the frame they are
/// transcript whatever their shape.
fn hostile_borders(widths: &[usize]) -> Vec<String> {
    let rule = |count: usize| RULE.to_string().repeat(count);
    let mut lines = vec![
        "\u{2500}\u{2500} \u{2807} Working".to_owned(),
        "\u{2500}\u{2500} \u{2807}".to_owned(),
        "\u{2500}\u{2500} \u{2807} Working \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".to_owned(),
        "\u{2500}\u{2500} \u{283c} Compacting context... (escape to cancel) \u{2500}\u{2500}"
            .to_owned(),
        "\u{2500}\u{2500} \u{2807} Retrying (1/3) in 1s... \u{2500}".to_owned(),
    ];
    for width in widths {
        lines.push(rule(*width));
        lines.push(format!(
            "\u{2500}\u{2500} \u{2807} Working {}",
            rule(width - 12)
        ));
        lines.push(format!(
            "{} \u{2191} 5 more {}",
            rule((width - 12) / 2),
            rule((width - 12) / 2)
        ));
    }
    lines
}

/// Small deterministic generator, so a failing screen reproduces.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).expect("bound")).expect("index")
    }

    fn pick<'a>(&mut self, list: &'a [String]) -> &'a str {
        &list[self.below(list.len())]
    }
}

struct Screen {
    lines: Vec<String>,
    width: usize,
    expected: Option<AgentActivity>,
}

/// One generated screen. `footer_len` outside `1..=4` leaves the frame without
/// a valid footer, which no rule may match.
#[allow(clippy::too_many_arguments, reason = "the generator's dimensions")]
fn assemble(
    random: &mut Random,
    width: usize,
    upper: &str,
    lower: &str,
    transcript_len: usize,
    draft_len: usize,
    footer_len: usize,
    pools: (&[String], &[String], &[String]),
) -> Screen {
    let (transcript_pool, draft_pool, footer_pool) = pools;
    let mut lines = Vec::new();
    for _ in 0..transcript_len {
        lines.push(random.pick(transcript_pool).to_owned());
    }
    lines.push(upper.to_owned());
    for _ in 0..draft_len {
        lines.push(random.pick(draft_pool).to_owned());
    }
    lines.push(lower.to_owned());
    for _ in 0..footer_len {
        lines.push(random.pick(footer_pool).to_owned());
    }
    let expected = (1..=4).contains(&footer_len).then(|| {
        if is_busy_border(upper) {
            AgentActivity::Working
        } else {
            AgentActivity::Idle
        }
    });
    Screen {
        lines,
        width,
        expected,
    }
}

fn generated_screens() -> Vec<Screen> {
    let captured = pieces();
    let widths: Vec<usize> = captured.upper.keys().copied().collect();
    let mut transcript_pool: Vec<String> = captured.transcript.clone();
    transcript_pool.extend(hostile_text());
    transcript_pool.extend(hostile_borders(&widths));
    transcript_pool.push(String::new());
    let mut draft_pool: Vec<String> = captured.draft.clone();
    draft_pool.extend(hostile_text());
    draft_pool.push(String::new());
    let mut footer_pool: Vec<String> = captured.footer.clone();
    footer_pool.extend(hostile_text());
    for line in transcript_pool.iter().filter(|line| is_border(line)).chain(
        draft_pool
            .iter()
            .chain(footer_pool.iter())
            .filter(|line| is_border(line)),
    ) {
        assert!(
            transcript_pool.contains(line)
                && !draft_pool.contains(line)
                && !footer_pool.contains(line),
            "border-shaped text stays above the frame: {line:?}"
        );
    }
    footer_pool.retain(|line| !line.is_empty());
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let mut screens = Vec::new();
    for (width, uppers) in &captured.upper {
        for upper in uppers {
            for lower in &captured.lower[width] {
                for transcript_len in 0..=5 {
                    for draft_len in 0..=7 {
                        for footer_len in [0, 1, 2, 3, 4, 5] {
                            for _ in 0..SEEDS_PER_COMBINATION {
                                screens.push(assemble(
                                    &mut random,
                                    *width,
                                    upper,
                                    lower,
                                    transcript_len,
                                    draft_len,
                                    footer_len,
                                    (&transcript_pool, &draft_pool, &footer_pool),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    screens
}

fn detector(manifest: &Manifest, columns: u16) -> Detector {
    let now = Instant::now();
    Detector::new(
        ROWS,
        columns,
        now,
        DetectorConfig {
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 1,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(manifest.clone()),
        },
    )
}

/// What a [`Detector`] publishes from the screen evidence of `lines`.
fn detected(manifest: &Manifest, lines: &[String], columns: usize) -> Option<AgentActivity> {
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(lines.join("\r\n").as_bytes());
    let now = Instant::now();
    let mut detector = detector(manifest, u16::try_from(columns).expect("columns"));
    detector
        .feed(now, &bytes)
        .iter()
        .rfind(|transition| transition.source == StateSource::Screen)
        .map(|transition| transition.activity)
}

#[test]
fn the_frame_rules_are_mutually_exclusive_and_follow_the_upper_border() {
    let manifests = manifests();
    let screens = generated_screens();
    assert!(
        screens.len() > 30_000,
        "generated screens: {}",
        screens.len()
    );
    let (mut busy, mut plain, mut unframed) = (0_usize, 0_usize, 0_usize);
    for (index, screen) in screens.iter().enumerate() {
        let working = read(&manifests.working, &screen.lines);
        let idle = read(&manifests.idle, &screen.lines);
        let context = format!(
            "screen {index} (width {}): {:#?}",
            screen.width, screen.lines
        );
        assert!(
            working.is_none() || idle.is_none(),
            "both rules match {context}"
        );
        assert_eq!(
            working.or(idle),
            screen.expected,
            "wrong classification {context}"
        );
        assert_eq!(
            read(&manifests.full, &screen.lines),
            screen.expected,
            "the full manifest disagrees {context}"
        );
        match screen.expected {
            Some(AgentActivity::Working) => {
                assert!(idle.is_none(), "a busy border read as idle {context}");
                busy += 1;
            }
            Some(_) => plain += 1,
            None => unframed += 1,
        }
        if index % END_TO_END_STRIDE == 0
            && screen.lines.len() <= usize::from(ROWS)
            && screen
                .lines
                .iter()
                .all(|line| line.chars().count() <= screen.width)
        {
            assert_eq!(
                detected(&manifests.full, &screen.lines, screen.width),
                screen.expected,
                "the detector disagrees {context}"
            );
        }
    }
    assert!(
        busy > 5000 && plain > 5000 && unframed > 5000,
        "every outcome is exercised: busy {busy}, plain {plain}, unframed {unframed}"
    );
}

#[test]
fn a_status_text_in_the_transcript_above_an_idle_editor_is_idle_only() {
    let manifests = manifests();
    let rule = RULE.to_string().repeat(100);
    let screen: Vec<String> = [
        "\u{2500}\u{2500} \u{2807} Working",
        "",
        rule.as_str(),
        "one draft line",
        rule.as_str(),
        "~/work",
        "0.0%/128k (auto)",
    ]
    .iter()
    .map(|line| (*line).to_owned())
    .collect();
    assert_eq!(read(&manifests.working, &screen), None);
    assert_eq!(read(&manifests.idle, &screen), Some(AgentActivity::Idle));
    assert_eq!(read(&manifests.full, &screen), Some(AgentActivity::Idle));
    assert_eq!(
        detected(&manifests.full, &screen, 100),
        Some(AgentActivity::Idle)
    );
}
