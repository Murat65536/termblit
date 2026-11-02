use std::process::{Command, Stdio};

use super::OutputMode;

/// The best mode the terminal is known to support, judged from environment
/// variables or tmux, without touching the terminal
pub fn detect() -> OutputMode {
    let env = |name: &str| std::env::var(name).ok();
    detect_with(env, || TmuxClient::query(env("TMUX_PANE")))
}

#[derive(Debug, Default, PartialEq)]
pub struct TmuxClient {
    // Like "xterm-kitty kitty(0.49.1)"
    terminal: String,
    // Like "256,RGB,sixel"
    features: String,
    sixel_support: bool,
    passthrough: bool,
}

const TMUX_FORMAT: &str = "#{client_termname} #{client_termtype}\t#{client_termfeatures}\t\
                           #{sixel_support}\t#{allow-passthrough}";

impl TmuxClient {
    pub fn query(pane: Option<String>) -> Option<Self> {
        let mut command = Command::new("tmux");
        command.args(["display-message", "-p"]);
        if let Some(pane) = pane {
            command.args(["-t", &pane]);
        }
        let output = command
            .arg(TMUX_FORMAT)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| Self::parse(&String::from_utf8_lossy(&output.stdout)))?
    }

    fn parse(reply: &str) -> Option<Self> {
        let mut fields = reply.trim_end_matches('\n').split('\t');
        let mut next = || fields.next();
        let (terminal, features, sixel_support, passthrough) = (next()?, next()?, next()?, next()?);
        Some(Self {
            terminal: terminal.to_lowercase(),
            features: features.into(),
            // Older versions leave out what they don't know about
            sixel_support: sixel_support == "1",
            passthrough: matches!(passthrough, "on" | "all"),
        })
    }

    fn has_feature(&self, name: &str) -> bool {
        self.features.split(',').any(|feature| feature == name)
    }
}

fn detect_with(
    env: impl Fn(&str) -> Option<String>,
    tmux: impl FnOnce() -> Option<TmuxClient>,
) -> OutputMode {
    // GNU screen and Neovim's terminal drop graphics
    if env("TMUX").is_some() {
        return tmux().map_or_else(
            || characters(&env, false),
            |client| inside_tmux(&env, &client),
        );
    }
    if env("STY").is_some() || env("NVIM").is_some() {
        return characters(&env, false);
    }

    let term = env("TERM").unwrap_or_default();
    let term_program = env("TERM_PROGRAM").unwrap_or_default();

    let kitty_graphics = env("KITTY_WINDOW_ID").is_some()
        || matches!(term.as_str(), "xterm-kitty" | "xterm-ghostty")
        || matches!(term_program.as_str(), "ghostty" | "WezTerm");
    if kitty_graphics {
        return OutputMode::Kitty;
    }

    let sixel = env("KONSOLE_VERSION").is_some()
        || ["foot", "mlterm", "contour", "yaft"]
            .iter()
            .any(|name| term.starts_with(name))
        || term_program == "iTerm.app";
    if sixel {
        return OutputMode::Sixel;
    }
    characters(&env, false)
}

fn inside_tmux(env: impl Fn(&str) -> Option<String>, client: &TmuxClient) -> OutputMode {
    // WezTerm doesn't support Kitty's Unicode placeholders
    if client.passthrough
        && ["kitty", "ghostty"]
            .iter()
            .any(|name| client.terminal.contains(name))
    {
        return OutputMode::Kitty;
    }
    // tmux draws Sixel images itself
    if client.sixel_support && client.has_feature("sixel") {
        return OutputMode::Sixel;
    }
    characters(env, client.has_feature("RGB"))
}

fn characters(env: impl Fn(&str) -> Option<String>, truecolor: bool) -> OutputMode {
    let term = env("TERM").unwrap_or_default();
    let colorterm = env("COLORTERM").unwrap_or_default();
    if truecolor
        || matches!(colorterm.as_str(), "truecolor" | "24bit")
        || env("WT_SESSION").is_some()
        || cfg!(windows)
    {
        return OutputMode::TruecolorCharacters;
    }
    if matches!(term.as_str(), "linux" | "vt100" | "vt220" | "dumb") {
        return OutputMode::BlockCharacters;
    }
    OutputMode::PaletteCharacters
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<_, _> = vars.iter().copied().collect();
        move |name| map.get(name).map(|value| value.to_string())
    }

    fn tmux_client(reply: &str) -> TmuxClient {
        TmuxClient::parse(reply).unwrap()
    }

    #[test]
    fn tmux_replies() {
        assert_eq!(
            tmux_client("xterm-kitty kitty(0.49.1)\t256,RGB,sixel\t1\ton\n"),
            TmuxClient {
                terminal: "xterm-kitty kitty(0.49.1)".into(),
                features: "256,RGB,sixel".into(),
                sixel_support: true,
                passthrough: true,
            }
        );
        // tmux before 3.3 knows neither Sixel nor passthrough
        let old = tmux_client("xterm-256color \t256\t\t\n");
        assert!(!old.sixel_support && !old.passthrough);
        assert_eq!(TmuxClient::parse("not a reply"), None);
    }

    #[test]
    fn detection_inside_multiplexers() {
        let in_tmux = env(&[
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TERM", "tmux-256color"),
        ]);
        let cases = [
            (
                "xterm-kitty kitty(0.49.1)\t256,RGB\t1\ton",
                OutputMode::Kitty,
            ),
            (
                "xterm-ghostty ghostty 1.2\t256,RGB\t1\tall",
                OutputMode::Kitty,
            ),
            // Kitty graphics need passthrough
            (
                "xterm-kitty kitty(0.49.1)\t256,RGB\t1\toff",
                OutputMode::TruecolorCharacters,
            ),
            ("foot foot(1.20)\t256,RGB,sixel\t1\toff", OutputMode::Sixel),
            // Sixel needs tmux built with it
            (
                "foot foot(1.20)\t256,RGB,sixel\t0\toff",
                OutputMode::TruecolorCharacters,
            ),
            (
                "wezterm WezTerm 2024\t256,RGB,sixel\t1\ton",
                OutputMode::Sixel,
            ),
        ];
        for (reply, expected) in cases {
            assert_eq!(
                detect_with(&in_tmux, || Some(tmux_client(reply))),
                expected,
                "{reply:?}"
            );
        }

        let kitty_in_tmux = env(&[
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("KITTY_WINDOW_ID", "1"),
            ("COLORTERM", "truecolor"),
        ]);
        assert_eq!(
            detect_with(kitty_in_tmux, || None),
            OutputMode::TruecolorCharacters
        );

        for (name, value) in [
            ("STY", "1234.pts-0.host"),
            ("NVIM", "/run/user/1000/nvim.1.0"),
        ] {
            let vars = [
                (name, value),
                ("TERM", "xterm-kitty"),
                ("COLORTERM", "truecolor"),
            ];
            assert_eq!(
                detect_with(env(&vars), || None),
                OutputMode::TruecolorCharacters,
                "{name}"
            );
        }
    }

    #[test]
    fn detection() {
        let cases: &[(&[(&str, &str)], OutputMode)] = &[
            (
                &[("KITTY_WINDOW_ID", "1"), ("TERM", "xterm-kitty")],
                OutputMode::Kitty,
            ),
            (&[("TERM_PROGRAM", "WezTerm")], OutputMode::Kitty),
            (&[("TERM", "xterm-ghostty")], OutputMode::Kitty),
            (&[("TERM", "foot")], OutputMode::Sixel),
            (
                &[("TERM", "xterm-256color"), ("KONSOLE_VERSION", "240401")],
                OutputMode::Sixel,
            ),
            (
                &[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")],
                OutputMode::TruecolorCharacters,
            ),
        ];
        for (vars, expected) in cases {
            assert_eq!(detect_with(env(vars), || None), *expected, "{vars:?}");
        }
        if cfg!(windows) {
            assert_eq!(
                detect_with(env(&[("TERM", "linux")]), || None),
                OutputMode::TruecolorCharacters
            );
            assert_eq!(
                detect_with(env(&[]), || None),
                OutputMode::TruecolorCharacters
            );
        } else {
            assert_eq!(
                detect_with(env(&[("TERM", "linux")]), || None),
                OutputMode::BlockCharacters
            );
            assert_eq!(
                detect_with(env(&[("TERM", "xterm-256color")]), || None),
                OutputMode::PaletteCharacters
            );
            assert_eq!(
                detect_with(env(&[]), || None),
                OutputMode::PaletteCharacters
            );
        }
    }
}
