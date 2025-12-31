use clap::{Args, CommandFactory};
use clap_complete::Shell as ClapShell;
use derive_more::derive::{Display, FromStr};

const EXE_NAME: &str = "tg";

#[derive(Args, Clone, Debug)]
pub struct ShellInitArgs {
	shell: Shell,
}

#[derive(Clone, Copy, Debug, Display, FromStr)]
enum Shell {
	Dash,
	Bash,
	Zsh,
	Fish,
}

impl Shell {
	fn aliases(&self) -> String {
		format!(
			r#"
# {EXE_NAME} aliases
function tga; {EXE_NAME} send -c alerts $argv 2>/dev/null; end
function tgg; {EXE_NAME} send -c general $argv 2>/dev/null; end
alias tgo="{EXE_NAME} open"
alias tgd="{EXE_NAME} todos open"
"#
		)
	}

	fn to_clap_shell(self) -> ClapShell {
		match self {
			Shell::Dash => ClapShell::Bash,
			Shell::Bash => ClapShell::Bash,
			Shell::Zsh => ClapShell::Zsh,
			Shell::Fish => ClapShell::Fish,
		}
	}

	fn completions(&self) -> String {
		let mut cmd = crate::Cli::command();
		let mut buffer = Vec::new();
		let shell = self.to_clap_shell();
		clap_complete::generate(shell, &mut cmd, EXE_NAME, &mut buffer);

		String::from_utf8(buffer).unwrap_or_else(|_| String::from("# Failed to generate completions"))
	}
}

pub fn output(args: ShellInitArgs) {
	let shell = args.shell;
	let s = format!("{}\n{}", shell.aliases(), shell.completions());
	println!("{s}");
}
