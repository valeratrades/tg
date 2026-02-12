use clap::{Args, CommandFactory};
use clap_complete::Shell as ClapShell;
use derive_more::derive::{Display, FromStr};

const EXE_NAME: &str = "tg";
#[derive(Args, Clone, Debug)]
pub struct ShellInitArgs {
	shell: Shell,
}
pub fn output(args: ShellInitArgs, has_alerts_channel: bool) {
	let shell = args.shell;
	let s = format!("{}\n{}", shell.aliases(has_alerts_channel), shell.completions());
	println!("{s}");
}
#[derive(Clone, Copy, Debug, Display, FromStr)]
enum Shell {
	Dash,
	Bash,
	Zsh,
	Fish,
}

impl Shell {
	fn aliases(&self, has_alerts_channel: bool) -> String {
		let alerts_alias = if has_alerts_channel {
			match self {
				Shell::Dash | Shell::Bash | Shell::Zsh => format!("\nalias tga='{EXE_NAME} send-alert'"),
				Shell::Fish => format!("\nalias tga '{EXE_NAME} send-alert'"),
			}
		} else {
			String::new()
		};

		match self {
			Shell::Dash | Shell::Bash | Shell::Zsh => {
				format!(
					r#"
# tg aliases
alias tgo='{EXE_NAME} open'
alias tgs='{EXE_NAME} send'
alias tgl='{EXE_NAME} list'
alias tgt='{EXE_NAME} todos open'{alerts_alias}"#
				)
			}
			Shell::Fish => {
				format!(
					r#"
# tg aliases
alias tgo '{EXE_NAME} open'
alias tgs '{EXE_NAME} send'
alias tgl '{EXE_NAME} list'
alias tgt '{EXE_NAME} todos open'{alerts_alias}"#
				)
			}
		}
	}

	fn completions(&self) -> String {
		let clap_shell = match self {
			Shell::Dash | Shell::Bash => ClapShell::Bash,
			Shell::Zsh => ClapShell::Zsh,
			Shell::Fish => ClapShell::Fish,
		};
		let mut buf = Vec::new();
		clap_complete::generate(clap_shell, &mut crate::Cli::command(), EXE_NAME, &mut buf);
		String::from_utf8(buf).unwrap_or_default()
	}
}
