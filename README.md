# Yolo and Solo Your Private Pool Server for ORE Mining

Forked from [ore-hq-server](https://github.com/Kriptikz/ore-hq-server.git). Tailored by Miraland Labs.

A lightweight release of Ore mining private pool server. Derived from and credited to ore-hq-server.

## Key Differentiators

**Simplified and lightweighted.**

**Optimized for private and/or personal use.**

**Zero charge for computing client, mining tx fee only for pool server.**

**No delegate.**

**No database.**

**Scale from a few to tens of devices, either laptop or PC.**

**Balance between worse penalties and better rewards.**

**Easy setup and flexible home deployment.**

## Install

To install the CLI, 2 approaches are recommended:

**Approach One**: install from crates.io directly, use [cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html):

```sh
cargo install ore-private-pool-srv
```

**Approach Two**: download source code from Github at: [github](https://github.com/miraland-labs/ore-private-pool-srv):

```sh
https://github.com/miraland-labs/ore-private-pool-srv
```

and then compile locally

`cargo build --release`

### Dependencies

If you run into issues during installation, please install the following dependencies for your operating system and try again:

#### Linux

```
sudo apt-get install openssl pkg-config libssl-dev
```

#### MacOS (using [Homebrew](https://brew.sh/))

```
brew install openssl pkg-config

# If you encounter issues with OpenSSL, you might need to set the following environment variables:
export PATH="/usr/local/opt/openssl/bin:$PATH"
export LDFLAGS="-L/usr/local/opt/openssl/lib"
export CPPFLAGS="-I/usr/local/opt/openssl/include"
```

#### Windows (using [Chocolatey](https://chocolatey.org/))

```
choco install openssl pkgconfiglite
```

## Build

To build the codebase from scratch, checkout the repo and use cargo to build:

```sh
cargo build --release
```

## Env Settings

copy `.env.example` file and rename to `.env`, set with your own settings.

## Run

To run pool server, execute:

```sh
ore-ppl-srv [OPTIONS]
```

or, if you build from source code downloaded from github, enter into ore-private-pool-srv home directory,
duplicate `bin.example` directory and rename to `bin`, modify `start-ore-ppl-srv.sh`, execute:

```
bin/start-ore-ppl-srv.sh
```

or

```
RUST_LOG=none,ore_ppl_srv=info bin/start-ore-ppl-srv.sh
```

you will find daily log file in `logs` sub directory.

## Help

You can use the `-h` flag on any command to pull up a help menu with documentation:

```sh
ore-ppl-srv -h
```

Usage: ore-ppl-srv [OPTIONS]

Options:
-b, --buffer-time <BUFFER_SECONDS>

The number seconds before the deadline to stop mining and start submitting. [default: 5]

-r, --risk-time <RISK_SECONDS>

Set extra hash time in seconds for miners to stop mining and start submitting, risking a penalty. [default: 0]

--priority-fee <FEE_MICROLAMPORTS>

Price to pay for compute units when dynamic fee flag is off, or dynamic fee is unavailable. [default: 100]

--priority-fee-cap <FEE_CAP_MICROLAMPORTS>

Max price to pay for compute units when dynamic fees are enabled. [default: 100000]

--dynamic-fee

Enable dynamic priority fees

--dynamic-fee-url <DYNAMIC_FEE_URL>

RPC URL to use for dynamic fee estimation.

-e, --expected-min-difficulty <EXPECTED_MIN_DIFFICULTY>

The expected min difficulty to submit from pool client. Reserved for potential qualification process unimplemented yet. [default: 8]

-e, --extra-fee-difficulty <EXTRA_FEE_DIFFICULTY>

The min difficulty that the pool server miner thinks deserves to pay more priority fee to land tx quickly. [default: 29]

-e, --extra-fee-percent <EXTRA_FEE_PERCENT>

The extra percentage that the pool server miner feels deserves to pay more of the priority fee. As a percentage, a multiple of 50 is recommended(example: 50, means pay extra 50% of the specified priority fee), and the final priority fee cannot exceed the priority fee cap. [default: 0]

-s, --slack-difficulty <SLACK_DIFFICULTY>

The min difficulty that will notify slack channel(if configured) upon transaction success. [default: 25]

--no-sound-notification

Sound notification on by default

## Support us | Donate at your discretion

We greatly appreciate any donation to help support projects development at Miraland Labs. Miraland is dedicated to freedom and individual sovereignty and we are doing our best to make it a reality.
Certainly, if you find this project helpful and would like to support its development, you can buy me/us a coffee!
Your support is greatly appreciated. It motivates me/us to keep improving this project.

**Bitcoin(BTC)**
`bc1plh7wnl0v0xfemmk395tvsu73jtt0s8l28lhhznafzrj5jwu4dy9qx2rpda`

![Donate BTC to Miraland Development](donations/donate-btc-qr-code.png)

**Solana(SOL)**
`9h9TXFtSsDAiL5kpCRZuKUxPE4Nv3W56fcSyUC3zmQip`

![Donate SOL to Miraland Development](donations/donate-sol-qr-code.png)

Thank you for your support!
