# Changelog

This project follows semantic versioning.

Possible log types:

-   `[added]` for new features.
-   `[changed]` for changes in existing functionality.
-   `[deprecated]` for once-stable features removed in upcoming releases.
-   `[removed]` for deprecated features removed in this release.
-   `[fixed]` for any bug fixes.
-   `[security]` to invite users to upgrade in case of vulnerabilities.

### v1.0.2 (2024-10-16)

-   [changed] Control log writing behavior with log level: info, warn, error, etc.
-   [changed] README.

### v1.0.1 (2024-10-12)

-   [fixed] infinite get_proof Err loop if a wallet has never mined before
-   [changed] replace some while-let with loop control.
-   [changed] log infos.

### v1.0.0 (2024-09-30)

-   [added] contribution log.
-   [added] stdout-log tracing layer.
-   [changed] refactor processors mod.
-   [changed] support client version V0 and V1.
-   [changed] README.

### v0.6.1 (2024-09-20)

-   [changed] Deps deadpool-sqlite git repo was changed to crates.io patch miraland-deadpool-sqlite 0.8.2.

### v0.6.0 (2024-09-20)

-   [added] Embedded lightweight database sqlite3 enabled by using the POWERED_BY_DBMS env variable.
-   [added] Experimental implementation of send and confirm transaction using TPU client. No guarantee for tx landing advantage yet.
-   [added] First implementation of a reporting system that provides miner summaries for the last 24 hours.
-   [added] Project local .cargo directory for config.toml
-   [added] CHANGELOG.
-   [changed] Some log infomation format and contents.
-   [changed] README.
-   [fixed] extra-fee-difficulty argument low bound inclusive

### v0.5.2 (2024-09-13)

-   [added] Deps bitflags = "2.6.0"
-   [changed] Use bitflags to represent messaging channel types.
-   [changed] Pay extra fee for precious diff with static mode.

### v0.5.1 (2024-09-09)

-   [added] Slack and discord notifications
-   [added] Deps erenity = "=0.11.7" for discord webhook
-   [added] Deps zeroize = "=1.3.0"
-   [changed] Downgrade and lock deps axum version = "=0.7.2" due to discord deps conflict with that of solana
-   [changed] Adjust info log contents with phrase wording, emoji.
-   [changed] Updated dependencies.
-   [deprecated] The slack_difficulty command arg. It's deprecated in favor of messaging_diff.
-   [fixed] Usage documentation in README.

### v0.5.0 (2024-09-07)

-   [added] Initial release since forked.
-   [removed] Deps about signup and delegate program and external database.
