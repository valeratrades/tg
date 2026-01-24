use clap::{Args, CommandFactory};
use clap_complete::Shell as ClapShell;
use derive_more::derive::{Display, FromStr};

#[derive(Args, Clone, Debug)]
pub struct ShellInitArgs {
	shell: Shell,
}
pub fn output(args: ShellInitArgs) {
	let shell = args.shell;
	let s = format!("{}\n{}", shell.aliases(), shell.completions());
	println!("{s}");
}
const EXE_NAME: &str = "tg";
#[derive(Clone, Copy, Debug, Display, FromStr)]
enum Shell {
	Dash,
	Bash,
	Zsh,
	Fish,
}

impl Shell {
	fn aliases(&self) -> String {
		match self {
			Shell::Dash | Shell::Bash | Shell::Zsh => {
				format!(
					r#"# tg aliases
alias tgo='{EXE_NAME} open'
alias tgs='{EXE_NAME} send'
alias tgl='{EXE_NAME} list'
alias tgt='{EXE_NAME} todos open'"#
				)
			}
			Shell::Fish => {
				format!(
					r#"# tg aliases
alias tgo '{EXE_NAME} open'
alias tgs '{EXE_NAME} send'
alias tgl '{EXE_NAME} list'
alias tgt '{EXE_NAME} todos open'"#
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
