use chrono::Local;
use std::{ops::Div, str::FromStr, sync::Arc, time::Duration};
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::{
    database::{Database, PoweredByDbms},
    utils::ORE_TOKEN_DECIMALS,
    MineConfig, POWERED_BY_DBMS,
};
pub async fn reporting_processor(
    interval_in_hrs: u64,
    mine_config: Arc<MineConfig>,
    database: Arc<Database>,
) {
    // initial report starts in 5 mins(300s)
    let mut time_to_next_reporting: u64 = 300;
    let mut timer = Instant::now();
    loop {
        let current_timestamp = timer.elapsed().as_secs();
        if current_timestamp.ge(&time_to_next_reporting) {
            let powered_by_dbms = POWERED_BY_DBMS.get_or_init(|| {
                let key = "POWERED_BY_DBMS";
                match std::env::var(key) {
                    Ok(val) => PoweredByDbms::from_str(&val)
                        .expect("POWERED_BY_DBMS must be set correctly."),
                    Err(_) => PoweredByDbms::Unavailable,
                }
            });
            if powered_by_dbms == &PoweredByDbms::Sqlite {
                info!(target: "server_log", "Preparing client summaries for last 24 hours.");
                let summaries_last_24_hrs =
                    database.get_summaries_for_last_24_hours(mine_config.pool_id).await;

                match summaries_last_24_hrs {
                    Ok(summaries) => {
                        // printing report header
                        let report_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                        let report_title = "Miner summaries for last 24 hours:";
                        info!(target: "server_log", "[{report_time}] {report_title}");
                        // println!("[{report_time}] {report_title}");
                        let line_header = format!(
                            "miner_pubkey     num_contributions   min_diff   avg_diff   max_diff   earning_sub_total   percent"
                        );
                        info!(target: "server_log", "{line_header}");
                        // println!("{line_header}");

                        let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                        for summary in summaries {
                            let mp = summary.miner_pubkey;
                            let len = mp.len();
                            let short_mp = format!("{}...{}", &mp[0..6], &mp[len - 4..len]);
                            let earned_rewards_dec =
                                (summary.earning_sub_total as f64).div(decimals);
                            let line = format!(
                                "{}    {:17}   {:8}   {:8}   {:8}       {:.11}   {:>6.2}%",
                                short_mp,
                                summary.num_of_contributions,
                                summary.min_diff,
                                summary.avg_diff,
                                summary.max_diff,
                                earned_rewards_dec,
                                summary.percent
                            );

                            info!(target: "server_log", "{line}");
                            // println!("{line}");
                        }
                    },
                    Err(e) => {
                        error!(target: "server_log", "Failed to prepare summary report: {e:?}");
                    },
                }
                time_to_next_reporting = interval_in_hrs * 3600; // in seconds
                timer = Instant::now();
            } else {
                warn!(target: "server_log", "Reporting system cannot be used when POWERED_BY_DBMS disabled. Exiting reporting system.");
                return;
            }
        } else {
            tokio::time::sleep(Duration::from_secs(
                time_to_next_reporting.saturating_sub(current_timestamp),
            ))
            .await;
        }
    }
}
