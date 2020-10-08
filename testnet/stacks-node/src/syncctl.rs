use std::collections::VecDeque;

use stacks::burnchains::Burnchain;
use stacks::chainstate::stacks::db::StacksChainState;
use stacks::util::get_epoch_time_secs;
use stacks::util::sleep_ms;

use crate::burnchains::BurnchainTip;

/// Monitor the state of the Stacks blockchain as the peer network and relay threads download and
/// proces Stacks blocks.  Don't allow the node to process the next PoX reward cycle's sortitions
/// unless it's reasonably sure that it has processed all Stacks blocks for this reward cycle.
/// This struct monitors the Stacks chainstate to make this determination.
pub struct PoxSyncWatchdog {
    /// number of attachable but unprocessed staging blocks over time
    new_attachable_blocks: VecDeque<i64>,
    /// number of newly-processed staging blocks over time
    new_processed_blocks: VecDeque<i64>,
    /// last time we asked for attachable blocks
    last_attachable_query: u64,
    /// last time we asked for processed blocks
    last_processed_query: u64,
    /// number of samples to take
    max_samples: u64,
    /// maximum number of blocks to count per query (affects performance!)
    max_staging: u64,
    /// when did we first start watching?
    watch_start_ts: u64,
    /// when did we first see a flatline in block-processing rate?
    last_block_processed_ts: u64,
    /// estimated time for a block to get downloaded.  Used to infer how long to wait for the first
    /// blocks to show up when waiting for this reward cycle.
    estimated_block_download_time: f64,
    /// estimated time for a block to get processed -- from when it shows up as attachable to when
    /// it shows up as processed.  Used to infer how long to wait for the last block to get
    /// processed before unblocking burnchain sync for the next reward cycle.
    estimated_block_process_time: f64,
    /// time between burnchain syncs in stead state
    steady_state_burnchain_sync_interval: u64,
    /// when to re-sync under steady state
    steady_state_resync_ts: u64,
    /// chainstate handle
    chainstate: StacksChainState,
}

impl PoxSyncWatchdog {
    pub fn new(
        mainnet: bool,
        chain_id: u32,
        chainstate_path: String,
        burnchain_poll_time: u64,
        download_timeout: u64,
    ) -> Result<PoxSyncWatchdog, String> {
        let (chainstate, _) = match StacksChainState::open(mainnet, chain_id, &chainstate_path) {
            Ok(cs) => cs,
            Err(e) => {
                return Err(format!(
                    "Failed to open chainstate at '{}': {:?}",
                    &chainstate_path, &e
                ));
            }
        };

        Ok(PoxSyncWatchdog {
            new_attachable_blocks: VecDeque::new(),
            new_processed_blocks: VecDeque::new(),
            last_attachable_query: 0,
            last_processed_query: 0,
            max_samples: download_timeout, // sample once per second for however long we expect a timeout to be
            max_staging: 10,
            watch_start_ts: 0,
            last_block_processed_ts: 0,
            estimated_block_download_time: download_timeout as f64,
            estimated_block_process_time: 5.0,
            steady_state_burnchain_sync_interval: burnchain_poll_time,
            steady_state_resync_ts: 0,
            chainstate: chainstate,
        })
    }

    /// How many recently-added Stacks blocks are in an attachable state, up to $max_staging?
    fn count_attachable_stacks_blocks(&mut self) -> Result<u64, String> {
        // number of staging blocks that have arrived since the last sortition
        let cnt = StacksChainState::count_attachable_staging_blocks(
            &self.chainstate.blocks_db,
            self.max_staging,
            self.last_attachable_query,
        )
        .map_err(|e| format!("Failed to count attachable staging blocks: {:?}", &e))?;

        self.last_attachable_query = get_epoch_time_secs();
        Ok(cnt)
    }

    /// How many recently-processed Stacks blocks are there, up to $max_staging?
    /// ($max_staging is necessary to limit the runtime of this method, since the underlying SQL
    /// uses COUNT(*), which in Sqlite is a _O(n)_ operation for _n_ rows)
    fn count_processed_stacks_blocks(&mut self) -> Result<u64, String> {
        // number of staging blocks that have arrived since the last sortition
        let cnt = StacksChainState::count_processed_staging_blocks(
            &self.chainstate.blocks_db,
            self.max_staging,
            self.last_processed_query,
        )
        .map_err(|e| format!("Failed to count attachable staging blocks: {:?}", &e))?;

        self.last_processed_query = get_epoch_time_secs();
        Ok(cnt)
    }

    /// Are we in the initial block download? i.e. is the burn tip snapshot far enough away
    /// from the burnchain height that we should be eagerly downloading snapshots?
    pub fn infer_initial_block_download(
        burnchain: &Burnchain,
        burnchain_tip: &BurnchainTip,
        burnchain_height: u64,
    ) -> bool {
        burnchain_tip.block_snapshot.block_height + (burnchain.stable_confirmations as u64)
            < burnchain_height
    }

    /// Calculate the first derivative of a list of points
    fn derivative(sample_list: &VecDeque<i64>) -> Vec<i64> {
        let mut deltas = vec![];
        let mut prev = 0;
        for (i, sample) in sample_list.iter().enumerate() {
            if i == 0 {
                prev = *sample;
                continue;
            }
            let delta = *sample - prev;
            prev = *sample;
            deltas.push(delta);
        }
        deltas
    }

    /// Is a derivative approximately flat, with a maximum absolute deviation from 0?
    /// Return whether or not the sample is mostly flat, and how many points were over the given
    /// error bar in either direction.
    fn is_mostly_flat(deriv: &Vec<i64>, error: i64) -> (bool, usize) {
        let mut total_deviates = 0;
        let mut ret = true;
        for d in deriv.iter() {
            if d.abs() > error {
                total_deviates += 1;
                ret = false;
            }
        }
        (ret, total_deviates)
    }

    /// low and high pass filter average -- take average without the smallest and largest values
    fn hilo_filter_avg(samples: &Vec<i64>) -> f64 {
        // take average with low and high pass
        let mut min = i64::max_value();
        let mut max = i64::min_value();
        for s in samples.iter() {
            if *s < 0 {
                // nonsensical result (e.g. due to clock drift?)
                continue;
            }
            if *s < min {
                min = *s;
            }
            if *s > max {
                max = *s;
            }
        }

        let mut count = 0;
        let mut sum = 0;
        for s in samples.iter() {
            if *s < 0 {
                // nonsensical result
                continue;
            }
            if *s == min {
                continue;
            }
            if *s == max {
                continue;
            }
            count += 1;
            sum += *s;
        }

        if count == 0 {
            // no viable samples
            1.0
        } else {
            (sum as f64) / (count as f64)
        }
    }

    /// estimate how long a block remains in an unprocessed state
    fn estimate_block_process_time(
        chainstate: &StacksChainState,
        burnchain: &Burnchain,
        tip_height: u64,
    ) -> f64 {
        let this_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_height)
            .expect(&format!("BUG: no reward cycle for {}", tip_height));
        let prev_reward_cycle = this_reward_cycle.saturating_sub(1);

        let start_height = burnchain.reward_cycle_to_block_height(prev_reward_cycle);
        let end_height = burnchain.reward_cycle_to_block_height(this_reward_cycle);

        if this_reward_cycle > 0 {
            assert!(start_height < end_height);
        } else {
            // no samples yet
            return 1.0;
        }

        let block_wait_times = StacksChainState::measure_block_wait_time(
            &chainstate.blocks_db,
            start_height,
            end_height,
        )
        .expect("BUG: failed to query chainstate block-processing times");

        PoxSyncWatchdog::hilo_filter_avg(&block_wait_times)
    }

    /// estimate how long a block takes to download
    fn estimate_block_download_time(
        chainstate: &StacksChainState,
        burnchain: &Burnchain,
        tip_height: u64,
    ) -> f64 {
        let this_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_height)
            .expect(&format!("BUG: no reward cycle for {}", tip_height));
        let prev_reward_cycle = this_reward_cycle.saturating_sub(1);

        let start_height = burnchain.reward_cycle_to_block_height(prev_reward_cycle);
        let end_height = burnchain.reward_cycle_to_block_height(this_reward_cycle);

        if this_reward_cycle > 0 {
            assert!(start_height < end_height);
        } else {
            // no samples yet
            return 1.0;
        }

        let block_download_times = StacksChainState::measure_block_download_time(
            &chainstate.blocks_db,
            start_height,
            end_height,
        )
        .expect("BUG: failed to query chainstate block-download times");

        PoxSyncWatchdog::hilo_filter_avg(&block_download_times)
    }

    /// Reset internal state.  Performed when it's okay to begin syncing the burnchain.
    /// Updates estimate for block-processing time and block-downloading time.
    fn reset(&mut self, burnchain: &Burnchain, tip_height: u64) {
        // find the average (with low/high pass filter) time a block spends in the DB without being
        // processed, during this reward cycle
        self.estimated_block_process_time =
            PoxSyncWatchdog::estimate_block_process_time(&self.chainstate, burnchain, tip_height);

        // find the average (with low/high pass filter) time a block spends downloading
        self.estimated_block_download_time =
            PoxSyncWatchdog::estimate_block_download_time(&self.chainstate, burnchain, tip_height);

        debug!(
            "Estimated block download time: {}s. Estimated block processing time: {}s",
            self.estimated_block_download_time, self.estimated_block_process_time
        );

        self.new_attachable_blocks.clear();
        self.new_processed_blocks.clear();
        self.last_block_processed_ts = 0;
        self.watch_start_ts = 0;
        self.steady_state_resync_ts = 0;
    }

    /// Wait until all of the Stacks blocks for the given reward cycle are seemingly downloaded and
    /// processed.  Do so by watching the _rate_ at which attachable Stacks blocks arrive and get
    /// processed.
    /// Returns whether or not we're still in the initial block download
    pub fn pox_sync_wait(
        &mut self,
        burnchain: &Burnchain,
        burnchain_tip: &BurnchainTip,
        burnchain_height: u64,
    ) -> bool {
        if self.watch_start_ts == 0 {
            self.watch_start_ts = get_epoch_time_secs();
        }
        if self.steady_state_resync_ts == 0 {
            self.steady_state_resync_ts =
                get_epoch_time_secs() + self.steady_state_burnchain_sync_interval;
        }

        // unconditionally download the first reward cycle
        if burnchain_tip.block_snapshot.block_height
            < burnchain.first_block_height + (burnchain.pox_constants.reward_cycle_length as u64)
        {
            debug!("PoX watchdog in first reward cycle -- sync immediately");
            return PoxSyncWatchdog::infer_initial_block_download(
                burnchain,
                burnchain_tip,
                burnchain_height,
            );
        }

        let mut steady_state = false;

        let ibd = loop {
            let ibd = PoxSyncWatchdog::infer_initial_block_download(
                burnchain,
                burnchain_tip,
                burnchain_height,
            );

            let expected_first_block_deadline =
                self.watch_start_ts + (self.estimated_block_download_time as u64);
            let expected_last_block_deadline = self.last_block_processed_ts
                + (self.estimated_block_download_time as u64)
                + (self.estimated_block_process_time as u64);

            match (
                self.count_attachable_stacks_blocks(),
                self.count_processed_stacks_blocks(),
            ) {
                (Ok(num_available), Ok(num_processed)) => {
                    self.new_attachable_blocks.push_back(num_available as i64);
                    self.new_processed_blocks.push_back(num_processed as i64);

                    if (self.new_attachable_blocks.len() as u64) > self.max_samples {
                        self.new_attachable_blocks.pop_front();
                    }
                    if (self.new_processed_blocks.len() as u64) > self.max_samples {
                        self.new_processed_blocks.pop_front();
                    }

                    if (self.new_attachable_blocks.len() as u64) < self.max_samples
                        || (self.new_processed_blocks.len() as u64) < self.max_samples
                    {
                        // still getting initial samples
                        if self.new_processed_blocks.len() % 10 == 0 {
                            debug!(
                                "PoX watchdog: Still warming up: {} out of {} samples...",
                                &self.new_attachable_blocks.len(),
                                &self.max_samples
                            );
                        }
                        sleep_ms(1000);
                        continue;
                    }

                    if self.watch_start_ts > 0
                        && get_epoch_time_secs() < expected_first_block_deadline
                    {
                        // still waiting for that first block in this reward cycle
                        debug!("PoX watchdog: Still warming up: waiting until {}s for first Stacks block download (estimated download time: {}s)...", expected_first_block_deadline, self.estimated_block_download_time);
                        sleep_ms(1000);
                        continue;
                    }

                    if self.watch_start_ts > 0
                        && (self.new_attachable_blocks.len() as u64) < self.max_samples
                        && self.watch_start_ts
                            + self.max_samples
                            + self.steady_state_burnchain_sync_interval
                                * (burnchain.stable_confirmations as u64)
                            < get_epoch_time_secs()
                    {
                        debug!(
                            "PoX watchdog: could not calculate {} samples in {} seconds.  Assuming suspend/resume, or assuming load is too high.", 
                            self.max_samples,
                            self.max_samples + self.steady_state_burnchain_sync_interval * (burnchain.stable_confirmations as u64)
                        );
                        self.reset(burnchain, burnchain_tip.block_snapshot.block_height);

                        self.watch_start_ts = get_epoch_time_secs();
                        self.steady_state_resync_ts =
                            get_epoch_time_secs() + self.steady_state_burnchain_sync_interval;
                        continue;
                    }

                    // take first derivative of samples -- see if the download and processing rate has gone to 0
                    let attachable_delta = PoxSyncWatchdog::derivative(&self.new_attachable_blocks);
                    let processed_delta = PoxSyncWatchdog::derivative(&self.new_processed_blocks);

                    let (flat_attachable, attachable_deviants) =
                        PoxSyncWatchdog::is_mostly_flat(&attachable_delta, 0);
                    let (flat_processed, processed_deviants) =
                        PoxSyncWatchdog::is_mostly_flat(&processed_delta, 0);

                    debug!("PoX watchdog: flat-attachable?: {}, flat-processed?: {}, estimated block-download time: {}s, estimated block-processing time: {}s",
                           flat_attachable, flat_processed, self.estimated_block_download_time, self.estimated_block_process_time);

                    if flat_attachable && flat_processed && self.last_block_processed_ts == 0 {
                        // we're flat-lining -- this may be the end of this cycle
                        self.last_block_processed_ts = get_epoch_time_secs();
                    }

                    if self.last_block_processed_ts > 0
                        && get_epoch_time_secs() < expected_last_block_deadline
                    {
                        debug!("PoX watchdog: Still processing blocks; waiting until at least min({},{})s before burnchain synchronization (estimated block-processing time: {}s)", 
                               get_epoch_time_secs() + 1, expected_last_block_deadline, self.estimated_block_process_time);
                        sleep_ms(1000);
                        continue;
                    }

                    if ibd {
                        // doing initial block download right now.
                        // only proceed to fetch the next reward cycle's burnchain blocks if we're neither downloading nor
                        // attaching blocks recently
                        debug!("PoX watchdog: In initial block download: flat-attachable = {}, flat-processed = {}, min-attachable: {}, min-processed: {}",
                               flat_attachable, flat_processed, &attachable_deviants, &processed_deviants);

                        if !flat_attachable || !flat_processed {
                            sleep_ms(1000);
                            continue;
                        }
                    } else {
                        let now = get_epoch_time_secs();
                        if now < self.steady_state_resync_ts {
                            // steady state
                            if !steady_state {
                                debug!("PoX watchdog: In steady-state; waiting until at least {} before burnchain synchronization", self.steady_state_resync_ts);
                                steady_state = true;
                            }
                            sleep_ms(1000);
                            continue;
                        }
                    }
                }
                (err_attach, err_processed) => {
                    // can only happen on DB query failure
                    error!("PoX watchdog: Failed to count recently attached ('{:?}') and/or processed ('{:?}') staging blocks", &err_attach, &err_processed);
                    panic!();
                }
            };

            self.reset(burnchain, burnchain_tip.block_snapshot.block_height);
            break ibd;
        };
        ibd
    }
}
