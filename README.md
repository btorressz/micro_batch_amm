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

### Algorithm:

- Collect all active orders for the current batch
- Test candidate prices (all limit prices from orders)
- For each price, compute bid volume (orders with limit_price >= price) and ask volume (orders with limit_price <= price)
- Select price that maximizes min(bid_volume, ask_volume)
- Match orders at that price using a greedy algorithm (sorted by price)
- Store clearing price and volumes in BatchState

---

  **⏱️ Timing:**

- Requires current_slot >= last_batch_slot + batch_duration_slots
- Optional keeper restrictions via keeper_restricted and only_keeper

  ---

#  Accounts:

- authority: Keeper or admin (signer)
- market: Market to clear
- batch_state: Initialized with clearing results
- remaining_accounts: Triplets of [Order, user_base_ata, user_quote_ata] for all orders in batch

    ---

###  settle_order
- Settles a single order after batch clearing.

  
    ---

### Behavior:

- Crossed Orders (limit price crosses clearing price):

- Bids: Receive base tokens, refund unused quote
- Asks: Receive quote tokens, no refund (full fill)
- All-or-nothing: Full amount_base_fp is matched or order is skipped


- Uncrossed Orders: Full refund of deposited tokens
- Deducts protocol fees from quote volume traded

### Accounts:

- user: Order owner (signer)
- market, batch_state, order: Order and batch context
- order_fill: Settlement record (initialized if needed)
- vault_base, vault_quote: Market vaults (sign transfers)
- user_base_ata, user_quote_ata: User's token accounts

  ### cancel_order
- Cancels an open order before the batch closes.

   ---

  ### **Requirements (Cancel Order)**

**Batch must still be open**  
`current_slot < last_batch_slot + batch_duration_slots`  
**Order must not be cancelled or filled**

---

### **Behavior**

- Refunds full deposit (base for asks, quote for bids)  
- Marks order as `cancelled = true`

---

### **Accounts**

- **user**: Order owner (signer)  
- **market, order**: Order to cancel  
- **vault_base, vault_quote**: Market vaults  
- **user_base_ata, user_quote_ata**: User's token accounts  

---

### **set_paused**

Pauses or unpauses the market (admin only).

**Parameters:**

- `paused`: `true` to pause, `false` to unpause  
- `pause_reason`: Numeric code (e.g., `1 = emergency`, `2 = maintenance`)  

---

### **set_params**

Updates risk and fee parameters (admin only).

**Parameters:**

- `new_fee_bps`: Updated fee basis points  
- `max_notional_per_batch_quote_fp`: Max quote notional per batch  
- `max_notional_per_user_per_batch_quote_fp`: Max quote notional per user per batch  
- `max_orders_global_per_batch`: Global order count cap  
- `max_price_move_bps`: Circuit breaker (max % price change from last clearing)  
- `keeper_fee_bps`: Keeper incentive fee  
- `min_base_order_fp`, `min_quote_order_fp`: Dust order minimums  
- `protocol_fee_bps`, `referral_fee_bps`: Fee split (protocol + referral ≤ new_fee_bps)  

---

### **view_market**

Emits a `MarketView` event with all key market parameters (for off-chain indexers / UIs).


---

## Data Structures

### **Market**
Global market state (**412 bytes**).

| Field | Type | Description |
|------|------|-------------|
| `authority` | `Pubkey` | Market admin |
| `base_mint`, `quote_mint` | `Pubkey` | Token mints |
| `vault_base`, `vault_quote` | `Pubkey` | PDA-owned token accounts |
| `batch_duration_slots` | `u64` | Batch time window |
| `current_batch_id` | `u64` | Current batch number |
| `last_batch_slot` | `u64` | Slot when last batch was cleared |
| `next_order_id` | `u64` | Monotonic order ID counter |
| `fee_bps` | `u16` | Total fee in basis points |
| `max_orders_per_user_per_batch` | `u32` | Per-user order cap |
| `paused` | `bool` | Emergency pause flag |
| `max_notional_per_batch_quote_fp` | `u128` | Batch notional cap (quote, 1e6) |
| `max_notional_per_user_per_batch_quote_fp` | `u128` | User notional cap (quote, 1e6) |
| `batch_notional_quote_fp` | `u128` | Current batch notional |
| `max_orders_global_per_batch` | `u32` | Global order cap |
| `global_orders_in_batch` | `u32` | Current batch order count |
| `max_price_move_bps` | `u16` | Circuit breaker (0 = disabled) |
| `last_clearing_price_fp` | `u64` | Last clearing price (1e6) |
| `keeper_fee_bps` | `u16` | Keeper incentive fee |
| `keeper_restricted` | `bool` | Restrict clearing to `only_keeper` |
| `only_keeper` | `Pubkey` | Whitelisted keeper (if restricted) |
| `protocol_fee_bps` | `u16` | Protocol fee split |
| `referral_fee_bps` | `u16` | Referral fee split |
| `protocol_fees_accrued_fp` | `u128` | Accrued protocol fees (1e6) |
| `min_base_order_fp`, `min_quote_order_fp` | `u64` | Dust order minimums |
| `pause_reason` | `u8` | Pause reason code |



---


### **Order**
Individual order (**107 bytes**).

| Field | Type | Description |
|-------|-------|-------------|
| `user` | `Pubkey` | Order owner |
| `market` | `Pubkey` | Parent market |
| `side` | `OrderSide` | Bid or Ask |
| `limit_price_fp` | `u64` | Limit price (1e6) |
| `amount_base_fp` | `u64` | Base amount (1e6) |
| `batch_id` | `u64` | Batch number |
| `filled` | `bool` | Settled flag |
| `cancelled` | `bool` | Cancelled flag |
| `quote_deposit_fp` | `u64` | Quote deposited (bids only) |
| `id` | `u64` | Unique order ID |


---

### **UserBatchStats**
Per-user-per-batch tracking (**93 bytes**).

| Field | Type | Description |
|-------|-------|-------------|
| `user` | `Pubkey` | User |
| `market` | `Pubkey` | Market |
| `batch_id` | `u64` | Batch number |
| `order_count` | `u32` | Orders placed by user |
| `notional_quote_fp` | `u128` | Total notional (1e6) |


---

### **BatchState**
Post-clearing batch summary (**161 bytes**).

| Field | Type | Description |
|-------|-------|-------------|
| `market` | `Pubkey` | Parent market |
| `batch_id` | `u64` | Batch number |
| `clearing_price_fp` | `u64` | Uniform clearing price (1e6) |
| `total_base_traded_fp` | `u64` | Total base volume matched |
| `total_quote_traded_fp` | `u64` | Total quote volume matched |
| `created_slot`, `cleared_slot` | `u64` | Creation and clearing slots |
| `settled` | `bool` | All orders settled flag |
| `keeper` | `Pubkey` | Keeper who cleared batch |
| `keeper_reward_quote_fp` | `u128` | Keeper fee earned |
| `remaining_base_to_settle_fp` | `u128` | Unsettled base volume |
| `remaining_quote_to_settle_fp` | `u128` | Unsettled quote volume |


---

### **OrderFill**
Settlement record (**73 bytes**).

| Field | Type | Description |
|-------|-------|-------------|
| `order` | `Pubkey` | Parent order |
| `batch_id` | `u64` | Batch number |
| `filled_base_fp` | `u64` | Base amount matched |
| `filled_quote_fp` | `u64` | Quote amount matched |
| `refund_base_fp` | `u64` | Base refunded |
| `refund_quote_fp` | `u64` | Quote refunded |
| `claimed` | `bool` | Settlement claimed flag |



---


## ⚠️ Risk Controls

### 🔒 **Notional Caps**
- **Per-batch cap (`max_notional_per_batch_quote_fp`)**  
  Limits total quote volume per batch  
- **Per-user cap (`max_notional_per_user_per_batch_quote_fp`)**  
  Prevents a single user from dominating a batch  

---

### 📉 **Order Count Limits**
- **Per-user (`max_orders_per_user_per_batch`)**  
  Limits number of orders one user can submit per batch  
- **Global (`max_orders_global_per_batch`)**  
  Limits total orders allowed in a batch  

---

### 🚨 **Price Band Circuit Breaker**
- **`max_price_move_bps`**: Max % deviation from `last_clearing_price_fp` (e.g., 500 = 5%)  
- Batch clearing **fails** if price moves beyond threshold  
- Set to **0** to disable  

---

### 🧹 **Dust Order Filters**
- **`min_base_order_fp`**: Minimum base size for asks  
- **`min_quote_order_fp`**: Minimum notional for bids  
- Prevents spam orders and unnecessary computation  

---

## 💰 Fee Structure

### 🏛️ **Protocol Fees**
- Deducted from quote volume on all matched trades  
- Accrued in **`protocol_fees_accrued_fp`** (withdrawable by admin)  
- Split into:  
  - `protocol_fee_bps` (treasury)  
  - `referral_fee_bps` (planned)  

---

### 🚀 **Keeper Fees**
- **`keeper_fee_bps`**: Incentive for keepers to clear batches  
- Stored in **`BatchState.keeper_reward_quote_fp`** (accounting only)  
- Withdrawable via future admin instruction  

---

### 📏 **Fee Constraints**
- `protocol_fee_bps + referral_fee_bps ≤ fee_bps ≤ 10,000` (100%)  

---

## 🔧 Keeper System

### 🟢 **Unrestricted Mode (Default)**
- Anyone can call `clear_batch` once `batch_duration_slots` has passed  
- Ideal for permissionless markets  

---

### 🔐 **Restricted Mode**
- Set `keeper_restricted = true`  
- Specify `only_keeper` pubkey  
- Only **that keeper** can clear batches  
- Useful for high-frequency or trusted partner operation  

---

### ⏱️ **Timing Guards**
- **`batch_duration_slots`**: Minimum delay before clearing  
- **`min_slots_between_clears`**: Additional buffer (e.g., keeper coordination)  

---

## 🔢 Fixed-Point Arithmetic (1e6)

All prices, amounts, and notionals use **1e6 precision** for deterministic math.

### 📘 Examples
- **Price**: `1,500,000` = **1.5 quote per base**  
- **Amount**: `2,000,000` = **2.0 base tokens**  
- **Quote required**:  
  `2,000,000 × 1,500,000 / 1,000,000 = 3,000,000` (3.0 quote)

---

### 🔄 Conversions

```rust
// User-facing → fixed-point
let price_fp = (price_decimal * 1_000_000.0) as u64;

// Fixed-point → user-facing
let price_decimal = price_fp as f64 / 1_000_000.0;

