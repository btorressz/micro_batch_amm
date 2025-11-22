use anchor_lang::prelude::*;
use anchor_lang::AccountDeserialize;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("8puhCTsdk8w61XfXTFVjr623BQWkq5NiBx4nyZ8FNffw");

const PRICE_SCALE: u64 = 1_000_000; // fixed-point scale for prices (1e6)
const BPS_DENOM: u64 = 10_000;      // basis points denominator

#[program]
pub mod micro_batch_amm {
    use super::*;

    /// Initialize a new market with base/quote mints and PDA token vaults.
    ///
    /// This is where we define the micro-batch parameters like duration and fee.
    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        batch_duration_slots: u64,
        fee_bps: u16,
        max_orders_per_user_per_batch: u32,
    ) -> Result<()> {
        require!(fee_bps as u64 <= BPS_DENOM, AmmError::InvalidFeeBps);

        let market = &mut ctx.accounts.market;
        market.authority = ctx.accounts.authority.key();
        market.base_mint = ctx.accounts.base_mint.key();
        market.quote_mint = ctx.accounts.quote_mint.key();
        market.vault_base = ctx.accounts.vault_base.key();
        market.vault_quote = ctx.accounts.vault_quote.key();
        market.batch_duration_slots = batch_duration_slots;
        market.last_batch_slot = 0;
        market.current_batch_id = 0;
        market.next_order_id = 0;
        market.fee_bps = fee_bps;
        market.max_orders_per_user_per_batch = max_orders_per_user_per_batch;
        market.paused = false;

        market.bump = ctx.bumps.market;
        market.vault_base_bump = ctx.bumps.vault_base;
        market.vault_quote_bump = ctx.bumps.vault_quote;

        // --- New risk / fee / keeper defaults ---

        // Notional caps (quote-side, fixed point 1e6)
        market.max_notional_per_batch_quote_fp = u128::MAX;
        market.max_notional_per_user_per_batch_quote_fp = u128::MAX;
        market.batch_notional_quote_fp = 0;

        market.max_orders_global_per_batch = u32::MAX;
        market.global_orders_in_batch = 0;

        // Price band (bps) & last price
        market.max_price_move_bps = 0; // 0 = disabled
        market.last_clearing_price_fp = 0;

        // Keeper incentives
        market.keeper_fee_bps = 0;
        market.keeper_treasury = ctx.accounts.authority.key();
        market.min_slots_between_clears = batch_duration_slots;
        market.keeper_restricted = false;
        market.only_keeper = Pubkey::default();

        // Protocol treasury / fees
        market.protocol_treasury = ctx.accounts.authority.key();
        market.referral_fee_bps = 0;
        market.protocol_fee_bps = fee_bps;
        market.protocol_fees_accrued_fp = 0;

        // Dust / min order sizes
        market.min_base_order_fp = 1;
        market.min_quote_order_fp = 1;

        // Pause reason code
        market.pause_reason = 0;

        emit!(MarketInitialized {
            market: market.key(),
            authority: market.authority,
            base_mint: market.base_mint,
            quote_mint: market.quote_mint,
            batch_duration_slots,
            fee_bps,
        });

        Ok(())
    }

    /// Place a new order into the current batch.
    ///
    /// - For Bids: user deposits **quote** tokens into the quote vault.
    /// - For Asks: user deposits **base** tokens into the base vault.
    ///
    /// `amount_base_fp` is the **amount of base** the user wants to trade, in fixed-point (1e6).
    /// For Bids we compute a max quote deposit = amount_base_fp * limit_price_fp / PRICE_SCALE.
    pub fn place_order(
        ctx: Context<PlaceOrder>,
        side: OrderSide,
        limit_price_fp: u64,
        amount_base_fp: u64,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(!market.paused, AmmError::MarketPaused);
        require!(limit_price_fp > 0, AmmError::InvalidPrice);
        require!(amount_base_fp > 0, AmmError::InvalidAmount);

        // Approx order notional in quote (fp)
        let order_notional_quote_fp: u128 = (amount_base_fp as u128)
            .checked_mul(limit_price_fp as u128)
            .ok_or(AmmError::MathOverflow)?
            / (PRICE_SCALE as u128);

        // Dust guards
        match side {
            OrderSide::Bid => {
                require!(
                    order_notional_quote_fp >= market.min_quote_order_fp as u128,
                    AmmError::DustOrderTooSmall
                );
            }
            OrderSide::Ask => {
                require!(
                    amount_base_fp as u128 >= market.min_base_order_fp as u128,
                    AmmError::DustOrderTooSmall
                );
            }
        }

        // Per-user-per-batch order count & notional caps
        let user_batch = &mut ctx.accounts.user_batch_stats;
        if user_batch.order_count == 0 {
            user_batch.user = ctx.accounts.user.key();
            user_batch.market = market.key();
            user_batch.batch_id = market.current_batch_id;
            user_batch.notional_quote_fp = 0;
            user_batch.bump = ctx.bumps.user_batch_stats;
        } else {
            require_keys_eq!(user_batch.user, ctx.accounts.user.key(), AmmError::InvalidUserBatch);
            require_keys_eq!(user_batch.market, market.key(), AmmError::InvalidUserBatch);
            require_eq!(user_batch.batch_id, market.current_batch_id, AmmError::InvalidUserBatch);
        }

        // User notional cap
        let new_user_notional = user_batch
            .notional_quote_fp
            .checked_add(order_notional_quote_fp)
            .ok_or(AmmError::MathOverflow)?;
        require!(
            new_user_notional <= market.max_notional_per_user_per_batch_quote_fp,
            AmmError::MaxNotionalPerUserExceeded
        );
        user_batch.notional_quote_fp = new_user_notional;

        // Per-user count
        require!(
            user_batch.order_count < market.max_orders_per_user_per_batch,
            AmmError::TooManyOrdersForUser
        );
        user_batch.order_count = user_batch
            .order_count
            .checked_add(1)
            .ok_or(AmmError::MathOverflow)?;

        // Global batch notional + global order count
        let new_batch_notional = market
            .batch_notional_quote_fp
            .checked_add(order_notional_quote_fp)
            .ok_or(AmmError::MathOverflow)?;
        require!(
            new_batch_notional <= market.max_notional_per_batch_quote_fp,
            AmmError::MaxNotionalPerBatchExceeded
        );
        market.batch_notional_quote_fp = new_batch_notional;

        require!(
            market.global_orders_in_batch < market.max_orders_global_per_batch,
            AmmError::MaxOrdersGlobalExceeded
        );
        market.global_orders_in_batch = market
            .global_orders_in_batch
            .checked_add(1)
            .ok_or(AmmError::MathOverflow)?;

        // Allocate order id
        let order_id = market.next_order_id;
        market.next_order_id = market
            .next_order_id
            .checked_add(1)
            .ok_or(AmmError::MathOverflow)?;

        let mut quote_deposit_fp: u64 = 0;

        match side {
            OrderSide::Bid => {
                // User wants to buy `amount_base_fp` of base at limit_price_fp.
                // We deposit max quote upfront.
                let quote_needed = ((amount_base_fp as u128)
                    .checked_mul(limit_price_fp as u128)
                    .ok_or(AmmError::MathOverflow)?
                    / PRICE_SCALE as u128) as u64;
                require!(quote_needed > 0, AmmError::InvalidAmount);
                quote_deposit_fp = quote_needed;

                // Transfer quote from user to vault_quote.
                let cpi_accounts = Transfer {
                    from: ctx.accounts.user_quote_ata.to_account_info(),
                    to: ctx.accounts.vault_quote.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                };
                let cpi_ctx =
                    CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
                token::transfer(cpi_ctx, quote_needed)?;
            }
            OrderSide::Ask => {
                // User wants to sell `amount_base_fp` of base.
                // Transfer base from user to vault_base.
                let cpi_accounts = Transfer {
                    from: ctx.accounts.user_base_ata.to_account_info(),
                    to: ctx.accounts.vault_base.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                };
                let cpi_ctx =
                    CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
                token::transfer(cpi_ctx, amount_base_fp)?;
            }
        }

        let order = &mut ctx.accounts.order;
        order.user = ctx.accounts.user.key();
        order.market = market.key();
        order.side = side;
        order.limit_price_fp = limit_price_fp;
        order.amount_base_fp = amount_base_fp;
        order.batch_id = market.current_batch_id;
        order.filled = false;
        order.cancelled = false;
        order.quote_deposit_fp = quote_deposit_fp;
        order.id = order_id;

        emit!(OrderPlaced {
            market: market.key(),
            order: order.key(),
            user: order.user,
            side,
            limit_price_fp,
            amount_base_fp,
            batch_id: order.batch_id,
        });

        Ok(())
    }

    /// Clear the current batch using a uniform clearing price.
    ///
    /// This ix computes the clearing price and volumes and rolls the batch.
    /// Token settlement happens later via `settle_order`.
    ///
    /// remaining_accounts = triplets: [Order, user_base_ata, user_quote_ata] * N
    pub fn clear_batch(ctx: Context<ClearBatch>) -> Result<()> {
        let clock = Clock::get()?;
        let remaining = &ctx.remaining_accounts;

        let market = &mut ctx.accounts.market;
        let batch_state = &mut ctx.accounts.batch_state;
        let authority = &ctx.accounts.authority;

        // Snapshot current batch info
        let (
            market_pk,
            current_batch_id,
            _fee_bps_u128,
            paused,
            last_batch_slot,
            batch_duration_slots,
            max_price_move_bps,
            last_clearing_price_fp,
            keeper_fee_bps,
        ) = {
            let mv = &*market;
            (
                mv.key(),
                mv.current_batch_id,
                mv.fee_bps as u128,
                mv.paused,
                mv.last_batch_slot,
                mv.batch_duration_slots,
                mv.max_price_move_bps,
                mv.last_clearing_price_fp,
                mv.keeper_fee_bps,
            )
        };

        require!(!paused, AmmError::MarketPaused);

        // Keeper gating
        if market.keeper_restricted {
            require_keys_eq!(
                market.only_keeper,
                authority.key(),
                AmmError::KeeperNotAllowed
            );
        }

        // Timing guard
        require!(
            clock.slot >= last_batch_slot + batch_duration_slots,
            AmmError::BatchNotReady
        );
        require!(
            clock.slot >= last_batch_slot + market.min_slots_between_clears,
            AmmError::BatchNotReady
        );

        require!(
            remaining.len() % 3 == 0,
            AmmError::InvalidRemainingAccountsLayout
        );

        // 1) Collect active orders for this batch.
        let mut temp_orders: Vec<TempOrder> = Vec::new();
        let mut candidate_prices: Vec<u64> = Vec::new();

        let mut idx = 0usize;
        while idx < remaining.len() {
            let order_ai = &remaining[idx];

            // Deserialize Order directly from account data.
            let mut data_slice: &[u8] = &order_ai.data.borrow();
            let order_acc: Order = Order::try_deserialize(&mut data_slice)?;

            if order_acc.market != market_pk
                || order_acc.batch_id != current_batch_id
                || order_acc.amount_base_fp == 0
                || order_acc.cancelled
            {
                idx += 3;
                continue;
            }

            temp_orders.push(TempOrder {
                account_index: idx,
                side: order_acc.side,
                limit_price_fp: order_acc.limit_price_fp,
                original_base_fp: order_acc.amount_base_fp as u128,
                remaining_base_fp: order_acc.amount_base_fp as u128,
                quote_deposit_fp: order_acc.quote_deposit_fp as u128,
            });

            if !candidate_prices.contains(&order_acc.limit_price_fp) {
                candidate_prices.push(order_acc.limit_price_fp);
            }

            idx += 3;
        }

        if temp_orders.is_empty() {
            // No orders in this batch; just roll batch.
            let cleared_batch_id = market.current_batch_id;
            market.last_batch_slot = clock.slot;
            market.current_batch_id = market
                .current_batch_id
                .checked_add(1)
                .ok_or(AmmError::MathOverflow)?;
            market.batch_notional_quote_fp = 0;
            market.global_orders_in_batch = 0;

            // Reset batch state
            batch_state.market = market_pk;
            batch_state.batch_id = cleared_batch_id;
            batch_state.clearing_price_fp = 0;
            batch_state.total_base_traded_fp = 0;
            batch_state.total_quote_traded_fp = 0;
            batch_state.created_slot = last_batch_slot;
            batch_state.cleared_slot = clock.slot;
            batch_state.settled = true; // trivially settled (no fills)
            batch_state.keeper = authority.key();
            batch_state.keeper_reward_quote_fp = 0;
            batch_state.remaining_base_to_settle_fp = 0;
            batch_state.remaining_quote_to_settle_fp = 0;

            emit!(BatchCleared {
                market: market_pk,
                batch_id: cleared_batch_id,
                clearing_price_fp: 0,
                total_base_traded_fp: 0,
                total_quote_traded_fp: 0,
            });
            return Ok(());
        }

        // 2) Find clearing price: maximize min(bid_volume, ask_volume).
        let mut best_price: u64 = 0;
        let mut best_traded: u128 = 0;

        for &p in candidate_prices.iter() {
            let mut bid_vol: u128 = 0;
            let mut ask_vol: u128 = 0;

            for o in temp_orders.iter() {
                match o.side {
                    OrderSide::Bid => {
                        if o.limit_price_fp >= p {
                            bid_vol = bid_vol
                                .checked_add(o.original_base_fp)
                                .ok_or(AmmError::MathOverflow)?;
                        }
                    }
                    OrderSide::Ask => {
                        if o.limit_price_fp <= p {
                            ask_vol = ask_vol
                                .checked_add(o.original_base_fp)
                                .ok_or(AmmError::MathOverflow)?;
                        }
                    }
                }
            }

            let traded = bid_vol.min(ask_vol);
            if traded > best_traded {
                best_traded = traded;
                best_price = p;
            }
        }

        if best_traded == 0 || best_price == 0 {
            // No price where bids and asks cross.
            let cleared_batch_id = market.current_batch_id;
            market.last_batch_slot = clock.slot;
            market.current_batch_id = market
                .current_batch_id
                .checked_add(1)
                .ok_or(AmmError::MathOverflow)?;
            market.batch_notional_quote_fp = 0;
            market.global_orders_in_batch = 0;

            batch_state.market = market_pk;
            batch_state.batch_id = cleared_batch_id;
            batch_state.clearing_price_fp = 0;
            batch_state.total_base_traded_fp = 0;
            batch_state.total_quote_traded_fp = 0;
            batch_state.created_slot = last_batch_slot;
            batch_state.cleared_slot = clock.slot;
            batch_state.settled = true;
            batch_state.keeper = authority.key();
            batch_state.keeper_reward_quote_fp = 0;
            batch_state.remaining_base_to_settle_fp = 0;
            batch_state.remaining_quote_to_settle_fp = 0;

            emit!(BatchCleared {
                market: market_pk,
                batch_id: cleared_batch_id,
                clearing_price_fp: 0,
                total_base_traded_fp: 0,
                total_quote_traded_fp: 0,
            });
            return Ok(());
        }

        let clearing_price_fp = best_price;

        // Price-band circuit breaker
        if last_clearing_price_fp > 0 && max_price_move_bps > 0 {
            let (high, low) = if clearing_price_fp >= last_clearing_price_fp {
                (clearing_price_fp, last_clearing_price_fp)
            } else {
                (last_clearing_price_fp, clearing_price_fp)
            };
            let delta = (high - low) as u128;
            let delta_bps = delta
                .checked_mul(BPS_DENOM as u128)
                .ok_or(AmmError::MathOverflow)?
                / (last_clearing_price_fp as u128);
            require!(
                delta_bps as u64 <= max_price_move_bps as u64,
                AmmError::PriceMoveTooLarge
            );
        }

        // 3) Build sorted indices: bids (desc price), asks (asc price).
        let mut bid_indices: Vec<usize> = Vec::new();
        let mut ask_indices: Vec<usize> = Vec::new();
        for (i, o) in temp_orders.iter().enumerate() {
            match o.side {
                OrderSide::Bid => bid_indices.push(i),
                OrderSide::Ask => ask_indices.push(i),
            }
        }

        bid_indices.sort_by(|&i, &j| {
            temp_orders[j]
                .limit_price_fp
                .cmp(&temp_orders[i].limit_price_fp)
        });
        ask_indices.sort_by(|&i, &j| {
            temp_orders[i]
                .limit_price_fp
                .cmp(&temp_orders[j].limit_price_fp)
        });

        let mut total_base_traded: u128 = 0;
        let mut total_quote_traded: u128 = 0;

        let mut bi = 0usize;
        let mut ai = 0usize;

        while bi < bid_indices.len() && ai < ask_indices.len() {
            let b_idx = bid_indices[bi];
            let a_idx = ask_indices[ai];

            // Only match orders that are crossed at clearing_price.
            let (bid_price, ask_price) = (
                temp_orders[b_idx].limit_price_fp,
                temp_orders[a_idx].limit_price_fp,
            );
            if bid_price < clearing_price_fp || ask_price > clearing_price_fp {
                break;
            }

            if temp_orders[b_idx].remaining_base_fp == 0 {
                bi += 1;
                continue;
            }
            if temp_orders[a_idx].remaining_base_fp == 0 {
                ai += 1;
                continue;
            }

            // Compute maximum base trade size for this pair.
            let mut trade_base_fp = temp_orders[b_idx]
                .remaining_base_fp
                .min(temp_orders[a_idx].remaining_base_fp);

            if trade_base_fp == 0 {
                break;
            }

            // For the bid, ensure we don't exceed quote deposit at clearing price.
            let bid_quote_deposit = temp_orders[b_idx].quote_deposit_fp;
            let max_base_affordable = (bid_quote_deposit * (PRICE_SCALE as u128))
                / (clearing_price_fp as u128).max(1);
            trade_base_fp = trade_base_fp.min(max_base_affordable);
            if trade_base_fp == 0 {
                bi += 1;
                continue;
            }

            let quote_gross = (trade_base_fp
                .checked_mul(clearing_price_fp as u128)
                .ok_or(AmmError::MathOverflow)?)
                / PRICE_SCALE as u128;

            if quote_gross == 0 {
                break;
            }

            temp_orders[b_idx].remaining_base_fp = temp_orders[b_idx]
                .remaining_base_fp
                .checked_sub(trade_base_fp)
                .ok_or(AmmError::MathOverflow)?;
            temp_orders[a_idx].remaining_base_fp = temp_orders[a_idx]
                .remaining_base_fp
                .checked_sub(trade_base_fp)
                .ok_or(AmmError::MathOverflow)?;

            total_base_traded = total_base_traded
                .checked_add(trade_base_fp)
                .ok_or(AmmError::MathOverflow)?;
            total_quote_traded = total_quote_traded
                .checked_add(quote_gross)
                .ok_or(AmmError::MathOverflow)?;

            if temp_orders[b_idx].remaining_base_fp == 0 {
                bi += 1;
            }
            if temp_orders[a_idx].remaining_base_fp == 0 {
                ai += 1;
            }
        }

        // Keeper reward (accounting only)
        let keeper_reward_quote_fp: u128 = if keeper_fee_bps > 0 {
            total_quote_traded
                .checked_mul(keeper_fee_bps as u128)
                .ok_or(AmmError::MathOverflow)?
                / (BPS_DENOM as u128)
        } else {
            0
        };

        // Final state update + event.
        let cleared_batch_id = market.current_batch_id;
        market.last_batch_slot = clock.slot;
        market.current_batch_id = market
            .current_batch_id
            .checked_add(1)
            .ok_or(AmmError::MathOverflow)?;
        market.batch_notional_quote_fp = 0;
        market.global_orders_in_batch = 0;
        market.last_clearing_price_fp = clearing_price_fp;

        // Update batch_state for settlement phase
        batch_state.market = market_pk;
        batch_state.batch_id = cleared_batch_id;
        batch_state.clearing_price_fp = clearing_price_fp;
        batch_state.total_base_traded_fp = total_base_traded as u64;
        batch_state.total_quote_traded_fp = total_quote_traded as u64;
        batch_state.created_slot = last_batch_slot;
        batch_state.cleared_slot = clock.slot;
        batch_state.settled = total_base_traded == 0;
        batch_state.keeper = authority.key();
        batch_state.keeper_reward_quote_fp = keeper_reward_quote_fp;
        batch_state.remaining_base_to_settle_fp = total_base_traded;
        batch_state.remaining_quote_to_settle_fp = total_quote_traded;

        emit!(BatchCleared {
            market: market_pk,
            batch_id: cleared_batch_id,
            clearing_price_fp,
            total_base_traded_fp: total_base_traded as u64,
            total_quote_traded_fp: total_quote_traded as u64,
        });

        Ok(())
    }

    /// Settle a single order after a batch has been cleared.
    ///
    /// This handles:
    /// - base/quote payouts
    /// - unused quote/base refunds
    /// - per-order fill record
    pub fn settle_order(ctx: Context<SettleOrder>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let batch_state = &mut ctx.accounts.batch_state;
        let order = &mut ctx.accounts.order;
        let order_fill = &mut ctx.accounts.order_fill;

        require!(!market.paused, AmmError::MarketPaused);
        require!(
            batch_state.market == market.key(),
            AmmError::BatchMarketMismatch
        );
        require!(
            batch_state.batch_id == order.batch_id,
            AmmError::BatchIdMismatch
        );
        require!(
            batch_state.clearing_price_fp > 0,
            AmmError::BatchNotCleared
        );
        require!(!order.cancelled, AmmError::OrderCancelled);
        require!(!order_fill.claimed, AmmError::OrderAlreadySettled);

        let price_fp = batch_state.clearing_price_fp as u128;
        let amount_base_fp_u128 = order.amount_base_fp as u128;
        let quote_deposit_fp_u128 = order.quote_deposit_fp as u128;

        // Check if order is crossed at clearing price
        let crossed = match order.side {
            OrderSide::Bid => order.limit_price_fp as u128 >= price_fp,
            OrderSide::Ask => order.limit_price_fp as u128 <= price_fp,
        };

        // Take local copies for seeds to avoid borrowing market immutably for the whole scope.
        let authority_key = market.authority;
        let base_mint_key = market.base_mint;
        let quote_mint_key = market.quote_mint;
        let bump = market.bump;

        // Helper seeds so vault PDAs can sign transfers
        let market_seeds: &[&[u8]] = &[
            b"market",
            authority_key.as_ref(),
            base_mint_key.as_ref(),
            quote_mint_key.as_ref(),
            &[bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[market_seeds];

        // Compute fill & refunds
        let mut filled_base_fp: u128 = 0;
        let mut filled_quote_fp: u128 = 0;
        let mut refund_base_fp: u128 = 0;
        let mut refund_quote_fp: u128 = 0;

        if crossed {
            // All-or-nothing settlement, constrained by remaining batch volume.
            require!(
                amount_base_fp_u128 <= batch_state.remaining_base_to_settle_fp,
                AmmError::BatchFullySettled
            );

            let gross_quote = amount_base_fp_u128
                .checked_mul(price_fp)
                .ok_or(AmmError::MathOverflow)?
                / (PRICE_SCALE as u128);

            require!(
                gross_quote <= quote_deposit_fp_u128 || matches!(order.side, OrderSide::Ask),
                AmmError::MathOverflow
            );

            match order.side {
                OrderSide::Bid => {
                    filled_base_fp = amount_base_fp_u128;
                    filled_quote_fp = gross_quote;
                    refund_base_fp = 0;
                    refund_quote_fp = quote_deposit_fp_u128
                        .checked_sub(gross_quote)
                        .ok_or(AmmError::MathOverflow)?;
                }
                OrderSide::Ask => {
                    filled_base_fp = amount_base_fp_u128;
                    filled_quote_fp = gross_quote;
                    refund_base_fp = 0; // full fill
                    refund_quote_fp = 0;
                }
            }

            // Update batch remaining volumes
            batch_state.remaining_base_to_settle_fp = batch_state
                .remaining_base_to_settle_fp
                .checked_sub(filled_base_fp)
                .ok_or(AmmError::MathOverflow)?;
            batch_state.remaining_quote_to_settle_fp = batch_state
                .remaining_quote_to_settle_fp
                .checked_sub(filled_quote_fp)
                .ok_or(AmmError::MathOverflow)?;

            if batch_state.remaining_base_to_settle_fp == 0 {
                batch_state.settled = true;
            }

            // Fee accounting (protocol only, referral bucket rolled into same for now)
            let protocol_fee_bps = market.protocol_fee_bps as u128;
            if protocol_fee_bps > 0 {
                let protocol_fee = filled_quote_fp
                    .checked_mul(protocol_fee_bps)
                    .ok_or(AmmError::MathOverflow)?
                    / (BPS_DENOM as u128);
                market.protocol_fees_accrued_fp = market
                    .protocol_fees_accrued_fp
                    .checked_add(protocol_fee)
                    .ok_or(AmmError::MathOverflow)?;
            }

            // Transfers
            let token_program_ai = ctx.accounts.token_program.to_account_info();

            match order.side {
                OrderSide::Bid => {
                    // BASE: vault_base -> user_base_ata
                    let cpi_accounts_base = Transfer {
                        from: ctx.accounts.vault_base.to_account_info(),
                        to: ctx.accounts.user_base_ata.to_account_info(),
                        authority: market.to_account_info(),
                    };
                    let cpi_ctx_base = CpiContext::new_with_signer(
                        token_program_ai.clone(),
                        cpi_accounts_base,
                        signer_seeds,
                    );
                    token::transfer(cpi_ctx_base, filled_base_fp as u64)?;

                    // QUOTE refund: vault_quote -> user_quote_ata
                    if refund_quote_fp > 0 {
                        let cpi_accounts_quote = Transfer {
                            from: ctx.accounts.vault_quote.to_account_info(),
                            to: ctx.accounts.user_quote_ata.to_account_info(),
                            authority: market.to_account_info(),
                        };
                        let cpi_ctx_quote = CpiContext::new_with_signer(
                            token_program_ai.clone(),
                            cpi_accounts_quote,
                            signer_seeds,
                        );
                        token::transfer(cpi_ctx_quote, refund_quote_fp as u64)?;
                    }
                }
                OrderSide::Ask => {
                    // QUOTE: vault_quote -> user_quote_ata
                    let cpi_accounts_quote = Transfer {
                        from: ctx.accounts.vault_quote.to_account_info(),
                        to: ctx.accounts.user_quote_ata.to_account_info(),
                        authority: market.to_account_info(),
                    };
                    let cpi_ctx_quote = CpiContext::new_with_signer(
                        token_program_ai.clone(),
                        cpi_accounts_quote,
                        signer_seeds,
                    );
                    token::transfer(cpi_ctx_quote, filled_quote_fp as u64)?;

                    // BASE refund (if any): vault_base -> user_base_ata
                    if refund_base_fp > 0 {
                        let cpi_accounts_base = Transfer {
                            from: ctx.accounts.vault_base.to_account_info(),
                            to: ctx.accounts.user_base_ata.to_account_info(),
                            authority: market.to_account_info(),
                        };
                        let cpi_ctx_base = CpiContext::new_with_signer(
                            token_program_ai,
                            cpi_accounts_base,
                            signer_seeds,
                        );
                        token::transfer(cpi_ctx_base, refund_base_fp as u64)?;
                    }
                }
            }
        } else {
            // Not crossed: pure refund.
            match order.side {
                OrderSide::Bid => {
                    refund_quote_fp = quote_deposit_fp_u128;
                    refund_base_fp = 0;
                }
                OrderSide::Ask => {
                    refund_base_fp = amount_base_fp_u128;
                    refund_quote_fp = 0;
                }
            }

            let token_program_ai = ctx.accounts.token_program.to_account_info();

            match order.side {
                OrderSide::Bid => {
                    // Quote refund only
                    if refund_quote_fp > 0 {
                        let cpi_accounts_quote = Transfer {
                            from: ctx.accounts.vault_quote.to_account_info(),
                            to: ctx.accounts.user_quote_ata.to_account_info(),
                            authority: market.to_account_info(),
                        };
                        let cpi_ctx_quote = CpiContext::new_with_signer(
                            token_program_ai,
                            cpi_accounts_quote,
                            signer_seeds,
                        );
                        token::transfer(cpi_ctx_quote, refund_quote_fp as u64)?;
                    }
                }
                OrderSide::Ask => {
                    // Base refund only
                    if refund_base_fp > 0 {
                        let cpi_accounts_base = Transfer {
                            from: ctx.accounts.vault_base.to_account_info(),
                            to: ctx.accounts.user_base_ata.to_account_info(),
                            authority: market.to_account_info(),
                        };
                        let cpi_ctx_base = CpiContext::new_with_signer(
                            token_program_ai,
                            cpi_accounts_base,
                            signer_seeds,
                        );
                        token::transfer(cpi_ctx_base, refund_base_fp as u64)?;
                    }
                }
            }
        }

        // Mark order + fill
        order.filled = true;

        order_fill.order = order.key();
        order_fill.batch_id = batch_state.batch_id;
        order_fill.filled_base_fp = filled_base_fp as u64;
        order_fill.filled_quote_fp = filled_quote_fp as u64;
        order_fill.refund_quote_fp = refund_quote_fp as u64;
        order_fill.refund_base_fp = refund_base_fp as u64;
        order_fill.claimed = true;

        emit!(OrderSettled {
            market: market.key(),
            order: order.key(),
            user: order.user,
            batch_id: batch_state.batch_id,
            side: order.side,
            clearing_price_fp: batch_state.clearing_price_fp,
            filled_base_fp: order_fill.filled_base_fp,
            filled_quote_fp: order_fill.filled_quote_fp,
            refund_base_fp: order_fill.refund_base_fp,
            refund_quote_fp: order_fill.refund_quote_fp,
        });

        Ok(())
    }

    /// Cancel an open order before the batch is cleared.
    ///
    /// - Refunds full deposit (base or quote)
    /// - Marks order as cancelled so clear_batch / settle_order ignore it.
    pub fn cancel_order(ctx: Context<CancelOrder>) -> Result<()> {
        let clock = Clock::get()?;
        let market = &mut ctx.accounts.market;
        let order = &mut ctx.accounts.order;

        require!(!market.paused, AmmError::MarketPaused);
        require!(!order.cancelled, AmmError::OrderCancelled);
        require!(!order.filled, AmmError::OrderAlreadySettled);

        // Batch must still be open
        require!(
            clock.slot < market.last_batch_slot + market.batch_duration_slots,
            AmmError::BatchAlreadyClosed
        );

        // Take local copies for seeds
        let authority_key = market.authority;
        let base_mint_key = market.base_mint;
        let quote_mint_key = market.quote_mint;
        let bump = market.bump;

        let token_program_ai = ctx.accounts.token_program.to_account_info();
        let market_seeds: &[&[u8]] = &[
            b"market",
            authority_key.as_ref(),
            base_mint_key.as_ref(),
            quote_mint_key.as_ref(),
            &[bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[market_seeds];

        // Simple full refund
        match order.side {
            OrderSide::Bid => {
                if order.quote_deposit_fp > 0 {
                    let cpi_accounts = Transfer {
                        from: ctx.accounts.vault_quote.to_account_info(),
                        to: ctx.accounts.user_quote_ata.to_account_info(),
                        authority: market.to_account_info(),
                    };
                    let cpi_ctx =
                        CpiContext::new_with_signer(token_program_ai, cpi_accounts, signer_seeds);
                    token::transfer(cpi_ctx, order.quote_deposit_fp)?;
                }
            }
            OrderSide::Ask => {
                if order.amount_base_fp > 0 {
                    let cpi_accounts = Transfer {
                        from: ctx.accounts.vault_base.to_account_info(),
                        to: ctx.accounts.user_base_ata.to_account_info(),
                        authority: market.to_account_info(),
                    };
                    let cpi_ctx =
                        CpiContext::new_with_signer(token_program_ai, cpi_accounts, signer_seeds);
                    token::transfer(cpi_ctx, order.amount_base_fp)?;
                }
            }
        }

        order.cancelled = true;

        emit!(OrderCancelled {
            market: market.key(),
            order: order.key(),
            user: order.user,
            batch_id: order.batch_id,
            side: order.side,
        });

        Ok(())
    }

    /// Pause/unpause a market and optionally set a pause reason code.
    pub fn set_paused(ctx: Context<SetPaused>, paused: bool, pause_reason: u8) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require_keys_eq!(market.authority, ctx.accounts.authority.key(), AmmError::Unauthorized);
        market.paused = paused;
        market.pause_reason = pause_reason;

        emit!(PausedSet {
            market: market.key(),
            paused,
            reason: pause_reason,
        });

        Ok(())
    }

    /// Admin function to tweak core risk and fee parameters.
    pub fn set_params(
        ctx: Context<SetParams>,
        new_fee_bps: u16,
        max_notional_per_batch_quote_fp: u128,
        max_notional_per_user_per_batch_quote_fp: u128,
        max_orders_global_per_batch: u32,
        max_price_move_bps: u16,
        keeper_fee_bps: u16,
        min_base_order_fp: u64,
        min_quote_order_fp: u64,
        protocol_fee_bps: u16,
        referral_fee_bps: u16,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require_keys_eq!(market.authority, ctx.accounts.authority.key(), AmmError::Unauthorized);

        require!(new_fee_bps as u64 <= BPS_DENOM, AmmError::InvalidFeeBps);
        require!(protocol_fee_bps as u64 <= new_fee_bps as u64, AmmError::InvalidFeeBps);
        require!(referral_fee_bps as u64 <= new_fee_bps as u64, AmmError::InvalidFeeBps);

        market.fee_bps = new_fee_bps;
        market.max_notional_per_batch_quote_fp = max_notional_per_batch_quote_fp;
        market.max_notional_per_user_per_batch_quote_fp = max_notional_per_user_per_batch_quote_fp;
        market.max_orders_global_per_batch = max_orders_global_per_batch;
        market.max_price_move_bps = max_price_move_bps;
        market.keeper_fee_bps = keeper_fee_bps;
        market.min_base_order_fp = min_base_order_fp;
        market.min_quote_order_fp = min_quote_order_fp;
        market.protocol_fee_bps = protocol_fee_bps;
        market.referral_fee_bps = referral_fee_bps;

        emit!(ParamsUpdated {
            market: market.key(),
            fee_bps: new_fee_bps,
            max_notional_per_batch_quote_fp,
            max_notional_per_user_per_batch_quote_fp,
            max_orders_global_per_batch,
            max_price_move_bps,
            keeper_fee_bps,
            min_base_order_fp,
            min_quote_order_fp,
            protocol_fee_bps,
            referral_fee_bps,
        });

        Ok(())
    }

    /// Simple read helper: emit key market params for off-chain UIs.
    pub fn view_market(ctx: Context<ViewMarket>) -> Result<()> {
        let market = &ctx.accounts.market;

        emit!(MarketView {
            market: market.key(),
            authority: market.authority,
            base_mint: market.base_mint,
            quote_mint: market.quote_mint,
            batch_duration_slots: market.batch_duration_slots,
            last_batch_slot: market.last_batch_slot,
            current_batch_id: market.current_batch_id,
            next_order_id: market.next_order_id,
            fee_bps: market.fee_bps,
            max_orders_per_user_per_batch: market.max_orders_per_user_per_batch,
            paused: market.paused,
            max_notional_per_batch_quote_fp: market.max_notional_per_batch_quote_fp,
            max_notional_per_user_per_batch_quote_fp: market.max_notional_per_user_per_batch_quote_fp,
            batch_notional_quote_fp: market.batch_notional_quote_fp,
            max_orders_global_per_batch: market.max_orders_global_per_batch,
            global_orders_in_batch: market.global_orders_in_batch,
            max_price_move_bps: market.max_price_move_bps,
            last_clearing_price_fp: market.last_clearing_price_fp,
            keeper_fee_bps: market.keeper_fee_bps,
            min_base_order_fp: market.min_base_order_fp,
            min_quote_order_fp: market.min_quote_order_fp,
            protocol_fee_bps: market.protocol_fee_bps,
            referral_fee_bps: market.referral_fee_bps,
            protocol_fees_accrued_fp: market.protocol_fees_accrued_fp,
            pause_reason: market.pause_reason,
        });

        Ok(())
    }
}

// -------------------------------
// Accounts
// -------------------------------

#[derive(Accounts)]
#[instruction(batch_duration_slots: u64, fee_bps: u16, max_orders_per_user_per_batch: u32)]
pub struct InitializeMarket<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    pub base_mint: Account<'info, Mint>,
    pub quote_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = authority,
        seeds = [
            b"market",
            authority.key().as_ref(),
            base_mint.key().as_ref(),
            quote_mint.key().as_ref()
        ],
        bump,
        space = 8 + Market::LEN
    )]
    pub market: Account<'info, Market>,

    #[account(
        init,
        payer = authority,
        seeds = [b"vault_base", market.key().as_ref()],
        bump,
        token::mint = base_mint,
        token::authority = market
    )]
    pub vault_base: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = authority,
        seeds = [b"vault_quote", market.key().as_ref()],
        bump,
        token::mint = quote_mint,
        token::authority = market
    )]
    pub vault_quote: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct PlaceOrder<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        has_one = base_mint,
        has_one = quote_mint,
        constraint = !market.paused
    )]
    pub market: Account<'info, Market>,

    pub base_mint: Account<'info, Mint>,
    pub quote_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = vault_base.key() == market.vault_base
    )]
    pub vault_base: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_quote.key() == market.vault_quote
    )]
    pub vault_quote: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_base_ata.owner == user.key(),
        constraint = user_base_ata.mint == base_mint.key()
    )]
    pub user_base_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_quote_ata.owner == user.key(),
        constraint = user_quote_ata.mint == quote_mint.key()
    )]
    pub user_quote_ata: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = user,
        seeds = [
            b"order",
            market.key().as_ref(),
            &market.next_order_id.to_le_bytes()
        ],
        bump,
        space = 8 + Order::LEN
    )]
    pub order: Account<'info, Order>,

    #[account(
        init_if_needed,
        payer = user,
        seeds = [
            b"user_batch",
            market.key().as_ref(),
            user.key().as_ref(),
            &market.current_batch_id.to_le_bytes()
        ],
        bump,
        space = 8 + UserBatchStats::LEN
    )]
    pub user_batch_stats: Account<'info, UserBatchStats>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct ClearBatch<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        mut,
        has_one = base_mint,
        has_one = quote_mint
    )]
    pub market: Account<'info, Market>,

    pub base_mint: Account<'info, Mint>,
    pub quote_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = vault_base.key() == market.vault_base
    )]
    pub vault_base: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_quote.key() == market.vault_quote
    )]
    pub vault_quote: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = authority,
        seeds = [b"batch_state", market.key().as_ref(), &market.current_batch_id.to_le_bytes()],
        bump,
        space = 8 + BatchState::LEN
    )]
    pub batch_state: Account<'info, BatchState>,

    pub token_program: Program<'info, Token>,
    // no #[account] attribute: avoids AccountDeserialize requirement
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SettleOrder<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,

    #[account(
        mut,
        has_one = market
    )]
    pub batch_state: Account<'info, BatchState>,

    #[account(
        mut,
        constraint = order.user == user.key(),
        constraint = order.market == market.key()
    )]
    pub order: Account<'info, Order>,

    #[account(
        init_if_needed,
        payer = user,
        seeds = [b"order_fill", order.key().as_ref()],
        bump,
        space = 8 + OrderFill::LEN
    )]
    pub order_fill: Account<'info, OrderFill>,

    #[account(
        mut,
        constraint = vault_base.key() == market.vault_base
    )]
    pub vault_base: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_quote.key() == market.vault_quote
    )]
    pub vault_quote: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_base_ata.owner == user.key(),
        constraint = user_base_ata.mint == market.base_mint
    )]
    pub user_base_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_quote_ata.owner == user.key(),
        constraint = user_quote_ata.mint == market.quote_mint
    )]
    pub user_quote_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    // no #[account] attribute
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelOrder<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,

    #[account(
        mut,
        constraint = order.user == user.key(),
        constraint = order.market == market.key()
    )]
    pub order: Account<'info, Order>,

    #[account(
        mut,
        constraint = vault_base.key() == market.vault_base
    )]
    pub vault_base: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_quote.key() == market.vault_quote
    )]
    pub vault_quote: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_base_ata.owner == user.key(),
        constraint = user_base_ata.mint == market.base_mint
    )]
    pub user_base_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_quote_ata.owner == user.key(),
        constraint = user_quote_ata.mint == market.quote_mint
    )]
    pub user_quote_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SetPaused<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
}

#[derive(Accounts)]
pub struct SetParams<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
}

#[derive(Accounts)]
pub struct ViewMarket<'info> {
    pub market: Account<'info, Market>,
}

// -------------------------------
// Data structs
// -------------------------------

#[account]
pub struct Market {
    pub authority: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub vault_base: Pubkey,
    pub vault_quote: Pubkey,

    pub batch_duration_slots: u64,
    pub last_batch_slot: u64,
    pub current_batch_id: u64,
    pub next_order_id: u64,

    pub fee_bps: u16,
    pub max_orders_per_user_per_batch: u32,
    pub paused: bool,

    pub bump: u8,
    pub vault_base_bump: u8,
    pub vault_quote_bump: u8,

    // --- Risk caps / notional ---
    pub max_notional_per_batch_quote_fp: u128,
    pub max_notional_per_user_per_batch_quote_fp: u128,
    pub batch_notional_quote_fp: u128,

    pub max_orders_global_per_batch: u32,
    pub global_orders_in_batch: u32,

    // --- Price band / last price ---
    pub max_price_move_bps: u16,
    pub last_clearing_price_fp: u64,

    // --- Keeper ---
    pub keeper_fee_bps: u16,
    pub keeper_treasury: Pubkey,
    pub min_slots_between_clears: u64,
    pub keeper_restricted: bool,
    pub only_keeper: Pubkey,

    // --- Fees / treasury ---
    pub protocol_treasury: Pubkey,
    pub referral_fee_bps: u16,
    pub protocol_fee_bps: u16,
    pub protocol_fees_accrued_fp: u128,

    // --- Dust limits ---
    pub min_base_order_fp: u64,
    pub min_quote_order_fp: u64,

    // --- Pause reason ---
    pub pause_reason: u8,
}

impl Market {
    pub const LEN: usize = 412;
}

#[account]
pub struct Order {
    pub user: Pubkey,
    pub market: Pubkey,
    pub side: OrderSide,
    pub limit_price_fp: u64,
    pub amount_base_fp: u64,
    pub batch_id: u64,
    pub filled: bool,
    pub cancelled: bool,
    pub quote_deposit_fp: u64,
    pub id: u64,
}

impl Order {
    pub const LEN: usize = 107;
}

#[account]
pub struct UserBatchStats {
    pub user: Pubkey,
    pub market: Pubkey,
    pub batch_id: u64,
    pub order_count: u32,
    pub bump: u8,
    pub notional_quote_fp: u128,
}

impl UserBatchStats {
    pub const LEN: usize = 93;
}

#[account]
pub struct BatchState {
    pub market: Pubkey,
    pub batch_id: u64,
    pub clearing_price_fp: u64,
    pub total_base_traded_fp: u64,
    pub total_quote_traded_fp: u64,
    pub created_slot: u64,
    pub cleared_slot: u64,
    pub settled: bool,
    pub keeper: Pubkey,
    pub keeper_reward_quote_fp: u128,
    pub remaining_base_to_settle_fp: u128,
    pub remaining_quote_to_settle_fp: u128,
}

impl BatchState {
    pub const LEN: usize = 161;
}

#[account]
pub struct OrderFill {
    pub order: Pubkey,
    pub batch_id: u64,
    pub filled_base_fp: u64,
    pub filled_quote_fp: u64,
    pub refund_quote_fp: u64,
    pub refund_base_fp: u64,
    pub claimed: bool,
}

impl OrderFill {
    pub const LEN: usize = 73;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Bid,
    Ask,
}

/// Local helper for in-memory order matching during batch clear.
struct TempOrder {
    pub account_index: usize, // index into remaining_accounts
    pub side: OrderSide,
    pub limit_price_fp: u64,
    pub original_base_fp: u128,
    pub remaining_base_fp: u128,
    pub quote_deposit_fp: u128,
}

// -------------------------------
// Events
// -------------------------------

#[event]
pub struct MarketInitialized {
    pub market: Pubkey,
    pub authority: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub batch_duration_slots: u64,
    pub fee_bps: u16,
}

#[event]
pub struct OrderPlaced {
    pub market: Pubkey,
    pub order: Pubkey,
    pub user: Pubkey,
    pub side: OrderSide,
    pub limit_price_fp: u64,
    pub amount_base_fp: u64,
    pub batch_id: u64,
}

#[event]
pub struct BatchCleared {
    pub market: Pubkey,
    pub batch_id: u64,
    pub clearing_price_fp: u64,
    pub total_base_traded_fp: u64,
    pub total_quote_traded_fp: u64,
}

#[event]
pub struct OrderCancelled {
    pub market: Pubkey,
    pub order: Pubkey,
    pub user: Pubkey,
    pub batch_id: u64,
    pub side: OrderSide,
}

#[event]
pub struct OrderSettled {
    pub market: Pubkey,
    pub order: Pubkey,
    pub user: Pubkey,
    pub batch_id: u64,
    pub side: OrderSide,
    pub clearing_price_fp: u64,
    pub filled_base_fp: u64,
    pub filled_quote_fp: u64,
    pub refund_base_fp: u64,
    pub refund_quote_fp: u64,
}

#[event]
pub struct PausedSet {
    pub market: Pubkey,
    pub paused: bool,
    pub reason: u8,
}

#[event]
pub struct ParamsUpdated {
    pub market: Pubkey,
    pub fee_bps: u16,
    pub max_notional_per_batch_quote_fp: u128,
    pub max_notional_per_user_per_batch_quote_fp: u128,
    pub max_orders_global_per_batch: u32,
    pub max_price_move_bps: u16,
    pub keeper_fee_bps: u16,
    pub min_base_order_fp: u64,
    pub min_quote_order_fp: u64,
    pub protocol_fee_bps: u16,
    pub referral_fee_bps: u16,
}

#[event]
pub struct MarketView {
    pub market: Pubkey,
    pub authority: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub batch_duration_slots: u64,
    pub last_batch_slot: u64,
    pub current_batch_id: u64,
    pub next_order_id: u64,
    pub fee_bps: u16,
    pub max_orders_per_user_per_batch: u32,
    pub paused: bool,
    pub max_notional_per_batch_quote_fp: u128,
    pub max_notional_per_user_per_batch_quote_fp: u128,
    pub batch_notional_quote_fp: u128,
    pub max_orders_global_per_batch: u32,
    pub global_orders_in_batch: u32,
    pub max_price_move_bps: u16,
    pub last_clearing_price_fp: u64,
    pub keeper_fee_bps: u16,
    pub min_base_order_fp: u64,
    pub min_quote_order_fp: u64,
    pub protocol_fee_bps: u16,
    pub referral_fee_bps: u16,
    pub protocol_fees_accrued_fp: u128,
    pub pause_reason: u8,
}

// -------------------------------
// Errors
// -------------------------------

#[error_code]
pub enum AmmError {
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Invalid fee bps")]
    InvalidFeeBps,
    #[msg("Market is paused")]
    MarketPaused,
    #[msg("Invalid price")]
    InvalidPrice,
    #[msg("Invalid amount")]
    InvalidAmount,
    #[msg("Batch not ready yet")]
    BatchNotReady,
    #[msg("Invalid remaining accounts layout")]
    InvalidRemainingAccountsLayout,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Too many orders for this user in the current batch")]
    TooManyOrdersForUser,
    #[msg("Invalid user batch account")]
    InvalidUserBatch,
    #[msg("Max notional per user exceeded")]
    MaxNotionalPerUserExceeded,
    #[msg("Max notional per batch exceeded")]
    MaxNotionalPerBatchExceeded,
    #[msg("Max global orders per batch exceeded")]
    MaxOrdersGlobalExceeded,
    #[msg("Dust order too small")]
    DustOrderTooSmall,
    #[msg("Price move too large for this batch")]
    PriceMoveTooLarge,
    #[msg("Keeper not allowed for this market")]
    KeeperNotAllowed,
    #[msg("Order already cancelled")]
    OrderCancelled,
    #[msg("Order already settled")]
    OrderAlreadySettled,
    #[msg("Batch already closed")]
    BatchAlreadyClosed,
    #[msg("Batch not cleared yet")]
    BatchNotCleared,
    #[msg("Batch fully settled")]
    BatchFullySettled,
    #[msg("Batch/market mismatch")]
    BatchMarketMismatch,
    #[msg("Batch id mismatch")]
    BatchIdMismatch,
}
