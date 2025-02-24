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
