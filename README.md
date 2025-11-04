# tg
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.92+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/tg.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/tg)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/tg)
![Lines Of Code](https://img.shields.io/badge/LoC-973-lightblue)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/valeratrades/tg/errors.yml?branch=master&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/valeratrades/tg/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/valeratrades/tg/warnings.yml?branch=master&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/valeratrades/tg/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->

`tg` tool allows you to interact with a Telegram bot to send messages, get bot information, and list channels configured in your configuration file. I use it to keep my quick notes.
<!-- markdownlint-disable -->
<details>
  <summary>
    <h3>Installation</h3>
  </summary>
<pre><code class="language-sh">nix build</code></pre>
</details>
<!-- markdownlint-restore -->

## Usage
### Commands
#### Send a Message
Send a message to a specified channel:
```sh
tg send -c channel2 "Today I'm feeling blue" "//this is still a part of the message"
```

#### Get Bot Information

Retrieve information about the bot:
```sh
tg bot-info
```

#### List Channels
List all the channels defined in the configuration file:
```sh
tg list-channels
```

#### Gen Aliases
An example of aliases to use with it. I myself gen them through this, then remove ones I don't intend to use often.
```sh
tg gen-aliases
```

### Configuration
Create a configuration file at `~/.config/tg.toml` (or specify a different path with the `--config` option) with the following structure:
```toml
[channels]
channel1 = "1234305221"
channel2 = "-1001234305221"
group_and_topic = "1234305221/7"
```

Each entry under `[channels]` represents a Telegram channel or group. Channels are specified by their ID, and groups are specified by their ID followed by a thread ID separated by a `/`.

Example config: ./examples/config.toml

#### Env
Ensure you have set the `TELEGRAM_BOT_KEY` environment variable with your Telegram bot API key:
```sh
export TELEGRAM_BOT_KEY="your_bot_api_key"
```



<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting).
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">Blue Oak 1.0.0</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>
