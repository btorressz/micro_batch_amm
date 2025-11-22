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
