# micro_batch_amm

### ⚙️ Micro-Batch AMM

A Solana program implementing a **micro-batch auction automated market maker (AMM)** with uniform clearing prices, risk controls, and keeper incentives.

---

### 📖 Overview

This AMM aggregates orders into discrete batches (time windows), computes a **uniform clearing price** that maximizes traded volume, and settles all matched orders at that single price. This design reduces MEV, provides fair execution, and supports sophisticated risk management.

---

### ✨ Key Features

- 🕒 **Batch Auctions**  
  Orders accumulate during a fixed time window (e.g., 100 slots), then clear at a single uniform price.

- 💵 **Uniform Clearing Price**  
  All matched trades execute at the same price, maximizing `min(bid_volume, ask_volume)`.

- 🛡️ **Risk Controls**  
  Notional caps per batch/user, order count limits, price band circuit breakers, and dust order filters.

- 💰 **Keeper Incentives**  
  Optional keeper fees and restrictions to incentivize timely batch clearing.

- 📈 **Fee Structure**  
  Protocol fees, planned referral fees, and keeper rewards.

- ❌ **Cancellation Support**  
  Users can cancel orders before the batch closes.

- 🔄 **Two-Phase Settlement**  
  1. Clear batch (compute price)  
  2. Settle individual orders.

---


### 🏗️ Architecture

---

### 🔄 Program Flow

1. **Initialize Market**  
   Create a market with base/quote token mints, vaults, and batch parameters.

2. **Place Orders**  
   Users deposit tokens (base for asks, quote for bids) and create limit orders.

3. **Clear Batch**  
   After `batch_duration_slots`, compute the clearing price and mark the batch as cleared.

4. **Settle Orders**  
   Users claim fills and refunds based on the clearing price.

5. **Cancel Orders**  
   Users can cancel before the batch clears to reclaim deposits.

---

### 🧾 Core Accounts

| Account         | Description                                                                 |
|-----------------|-----------------------------------------------------------------------------|
| `Market`        | Global market state (mints, vaults, batch config, risk parameters)         |
| `Order`         | Individual order with side, limit price, amount, and batch ID              |
| `UserBatchStats`| Per-user-per-batch order count and notional tracking                       |
| `BatchState`    | Post-clearing state (clearing price, volumes, settlement status)           |
| `OrderFill`     | Settlement record (fills, refunds) for each order                          |

---

### 🛠️ Instructions

#### 🔧 `initialize_market`

Creates a new market with base/quote mints and PDA-owned token vaults.

**Parameters:**
- `batch_duration_slots`: Time window for order collection (e.g., 100 slots)
- `fee_bps`: Initial fee in basis points (e.g., 30 = 0.30%)
- `max_orders_per_user_per_batch`: Per-user order limit

**Accounts:**
- `authority`: Market admin (signer)
- `base_mint`, `quote_mint`: SPL token mints
- `market`: PDA initialized with market state
- `vault_base`, `vault_quote`: Token accounts owned by market PDA

---

#### 📝 `place_order`

Places a new order into the current batch.

**Parameters:**
- `side`: `Bid` (buy base with quote) or `Ask` (sell base for quote)
- `limit_price_fp`: Max price for bids, min price for asks (fixed-point, 1e6 scale)
- `amount_base_fp`: Base token amount to trade (fixed-point, 1e6)

**Behavior:**
- **Bids:** Deposits `amount_base_fp * limit_price_fp / 1e6` quote tokens into vault
- **Asks:** Deposits `amount_base_fp` base tokens into vault
- Enforces dust limits, notional caps, and per-user order count limits

**Accounts:**
- `user`: Order placer (signer)
- `market`: Target market
- `order`: New order PDA
- `user_batch_stats`: Per-user batch tracking
- `user_base_ata`, `user_quote_ata`: User's token accounts
- `vault_base`, `vault_quote`: Market vaults

---

#### 🧮 `clear_batch`

Computes the **uniform clearing price** and rolls to the next batch.

---

