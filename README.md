# tg
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.82+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/tg.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/tg)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/tg)
[<img alt="build status" src="https://img.shields.io/github/actions/workflow/status/valeratrades/tg/ci.yml?branch=master&style=for-the-badge&style=flat-square" height="20">](https://github.com/valeratrades/tg/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->
![Lines Of Code](https://img.shields.io/badge/LoC-603-lightblue)

<!-- markdownlint-disable -->
<details>
  <summary>
    <h2>Installation<h2>
  </summary>

# README is out of date and I can't be bothered

To install the `tg` crate, ensure you have the nightly version of Rust (1.82+). You can install the crate via `cargo` with the following command:

```sh
cargo install tg
```
</details>
<!-- markdownlint-restore -->

## Usage
The `tg` tool allows you to interact with a Telegram bot to send messages, get bot information, and list channels configured in your configuration file.

### Configuration
Create a configuration file at `~/.config/tg.toml` (or specify a different path with the `--config` option) with the following structure:
```toml
[channels]
channel1 = "1234305221"
channel2 = "-1001234305221"
group_and_topic = "1234305221/7"
```

Each entry under `[channels]` represents a Telegram channel or group. Channels are specified by their ID, and groups are specified by their ID followed by a thread ID separated by a `/`.

### Environment Variable
Ensure you have set the `TELEGRAM_BOT_KEY` environment variable with your Telegram bot API key:
```sh
export TELEGRAM_BOT_KEY="your_bot_api_key"
```

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


### License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

<br>

<sup>
This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a>.
</sup>

