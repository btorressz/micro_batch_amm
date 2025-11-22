// In Solana Playground, web3, anchor, pg, BN, assert are globally available.

import * as splToken from "@solana/spl-token";

describe("micro_batch_amm", () => {
  const BN = anchor.BN;

  it("initialize market, place bid + ask, clear batch, settle bid order", async () => {
    const connection = pg.connection;
    const wallet = pg.wallet;

    // ----------------------------------------
    // 1) Resolve payer keypair
    // ----------------------------------------
    // @ts-ignore - Playground exposes an underlying Keypair
    const payer: web3.Keypair = wallet.payer ?? wallet.keypair;

    // ----------------------------------------
    // 2) Create base & quote mints + user ATAs
    // ----------------------------------------

    // 6 decimals for both mints (1e6 fixed point)
    const baseMint = await splToken.createMint(
      connection,
      payer,                // payer
      wallet.publicKey,     // mint authority
      null,                 // freeze authority
      6                     // decimals
    );

    const quoteMint = await splToken.createMint(
      connection,
      payer,
      wallet.publicKey,
      null,
      6
    );

    // User ATAs for base & quote
    const userBaseAta = await splToken.getOrCreateAssociatedTokenAccount(
      connection,
      payer,
      baseMint,
      wallet.publicKey
    );

    const userQuoteAta = await splToken.getOrCreateAssociatedTokenAccount(
      connection,
      payer,
      quoteMint,
      wallet.publicKey
    );

    // Mint some tokens so user can both buy and sell
    await splToken.mintTo(
      connection,
      payer,
      baseMint,
      userBaseAta.address,
      wallet.publicKey,
      BigInt(1_000_000_000) // 1,000 base (with 6 decimals)
    );

    await splToken.mintTo(
      connection,
      payer,
      quoteMint,
      userQuoteAta.address,
      wallet.publicKey,
      BigInt(10_000_000_000) // 10,000 quote (with 6 decimals)
    );

    // ----------------------------------------
    // 3) Derive PDAs (market + vaults)
    // ----------------------------------------

    const programId = pg.program.programId;

    const [marketPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("market"),
        wallet.publicKey.toBuffer(),
        baseMint.toBuffer(),
        quoteMint.toBuffer(),
      ],
      programId
    );

    const [vaultBasePda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("vault_base"), marketPda.toBuffer()],
      programId
    );

    const [vaultQuotePda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("vault_quote"), marketPda.toBuffer()],
      programId
    );

    // ----------------------------------------
    // 4) initializeMarket
    // ----------------------------------------

    const batchDurationSlots = new BN(5); // small batch duration for tests
    const feeBps = 50;                    // 0.50%
    const maxOrdersPerUserPerBatch = 10;

    const txInit = await pg.program.methods
      .initializeMarket(batchDurationSlots, feeBps, maxOrdersPerUserPerBatch)
      .accounts({
        authority: wallet.publicKey,
        baseMint,
        quoteMint,
        market: marketPda,
        vaultBase: vaultBasePda,
        vaultQuote: vaultQuotePda,
        systemProgram: web3.SystemProgram.programId,
        tokenProgram: splToken.TOKEN_PROGRAM_ID,
        rent: web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    console.log("initializeMarket tx:", txInit);

    const marketAccount = await pg.program.account.market.fetch(marketPda);
    assert.equal(marketAccount.authority.toBase58(), wallet.publicKey.toBase58());
    assert.equal(marketAccount.baseMint.toBase58(), baseMint.toBase58());
    assert.equal(marketAccount.quoteMint.toBase58(), quoteMint.toBase58());
    assert.equal(marketAccount.currentBatchId.toNumber(), 0);

    // ----------------------------------------
    // 5) placeOrder - Bid
    // ----------------------------------------

    const marketBeforeBid = await pg.program.account.market.fetch(marketPda);
    const nextOrderIdBid: anchor.BN = marketBeforeBid.nextOrderId;
    const currentBatchId: anchor.BN = marketBeforeBid.currentBatchId;

    const [orderBidPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("order"),
        marketPda.toBuffer(),
        nextOrderIdBid.toArrayLike(Buffer, "le", 8),
      ],
      programId
    );

    const [userBatchStatsPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("user_batch"),
        marketPda.toBuffer(),
        wallet.publicKey.toBuffer(),
        currentBatchId.toArrayLike(Buffer, "le", 8),
      ],
      programId
    );

    const sideBid = { bid: {} };
    const limitPriceFp = new BN(1_000_000); // price = 1.0
    const amountBaseFp = new BN(1_000_000); // 1 base unit (fp)

    const txPlaceBid = await pg.program.methods
      .placeOrder(sideBid, limitPriceFp, amountBaseFp)
      .accounts({
        user: wallet.publicKey,
        market: marketPda,
        baseMint,
        quoteMint,
        vaultBase: vaultBasePda,
        vaultQuote: vaultQuotePda,
        userBaseAta: userBaseAta.address,
        userQuoteAta: userQuoteAta.address,
        order: orderBidPda,
        userBatchStats: userBatchStatsPda,
        systemProgram: web3.SystemProgram.programId,
        tokenProgram: splToken.TOKEN_PROGRAM_ID,
        rent: web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    console.log("placeOrder (bid) tx:", txPlaceBid);

    const orderBidAccount = await pg.program.account.order.fetch(orderBidPda);
    assert.equal(orderBidAccount.user.toBase58(), wallet.publicKey.toBase58());
    assert.equal(orderBidAccount.market.toBase58(), marketPda.toBase58());
    assert.deepEqual(orderBidAccount.side, sideBid);
    assert.ok(orderBidAccount.amountBaseFp.eq(amountBaseFp));
    assert.ok(orderBidAccount.limitPriceFp.eq(limitPriceFp));
    assert.equal(orderBidAccount.filled, false);
    assert.equal(orderBidAccount.cancelled, false);

    // ----------------------------------------
    // 6) placeOrder - Ask (same user, same price/size)
    //    This ensures there's an actual crossing trade.
    // ----------------------------------------

    const marketBeforeAsk = await pg.program.account.market.fetch(marketPda);
    const nextOrderIdAsk: anchor.BN = marketBeforeAsk.nextOrderId;

    const [orderAskPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("order"),
        marketPda.toBuffer(),
        nextOrderIdAsk.toArrayLike(Buffer, "le", 8),
      ],
      programId
    );

    const sideAsk = { ask: {} };

    const txPlaceAsk = await pg.program.methods
      .placeOrder(sideAsk, limitPriceFp, amountBaseFp)
      .accounts({
        user: wallet.publicKey,
        market: marketPda,
        baseMint,
        quoteMint,
        vaultBase: vaultBasePda,
        vaultQuote: vaultQuotePda,
        userBaseAta: userBaseAta.address,
        userQuoteAta: userQuoteAta.address,
        order: orderAskPda,
        userBatchStats: userBatchStatsPda, // same user_batch PDA, already initialized
        systemProgram: web3.SystemProgram.programId,
        tokenProgram: splToken.TOKEN_PROGRAM_ID,
        rent: web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    console.log("placeOrder (ask) tx:", txPlaceAsk);

    const orderAskAccount = await pg.program.account.order.fetch(orderAskPda);
    assert.deepEqual(orderAskAccount.side, sideAsk);
    assert.ok(orderAskAccount.amountBaseFp.eq(amountBaseFp));
    assert.ok(orderAskAccount.limitPriceFp.eq(limitPriceFp));

    // ----------------------------------------
    // 7) clearBatch
    // ----------------------------------------

    const marketForClear = await pg.program.account.market.fetch(marketPda);
    const batchIdForClear: anchor.BN = marketForClear.currentBatchId;

    const [batchStatePda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("batch_state"),
        marketPda.toBuffer(),
        batchIdForClear.toArrayLike(Buffer, "le", 8),
      ],
      programId
    );

    // We only need to pass one triplet of remaining accounts for now
    const txClear = await pg.program.methods
      .clearBatch()
      .accounts({
        authority: wallet.publicKey,
        market: marketPda,
        baseMint,
        quoteMint,
        vaultBase: vaultBasePda,
        vaultQuote: vaultQuotePda,
        batchState: batchStatePda,
        tokenProgram: splToken.TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .remainingAccounts([
        {
          pubkey: orderBidPda,
          isSigner: false,
          isWritable: true,
        },
        {
          pubkey: userBaseAta.address,
          isSigner: false,
          isWritable: false,
        },
        {
          pubkey: userQuoteAta.address,
          isSigner: false,
          isWritable: false,
        },
        // (Optionally  could also add the ask order triplet if you extend
        // clearBatch logic to use them, but current Rust code only reads the
        // Order account itself.)
      ])
      .rpc();

    console.log("clearBatch tx:", txClear);

    const batchStateAccount = await pg.program.account.batchState.fetch(
      batchStatePda
    );

    assert.equal(batchStateAccount.market.toBase58(), marketPda.toBase58());
    // Now that there's an actual crossing, clearingPrice should be > 0
    assert.ok(batchStateAccount.clearingPriceFp.gt(new BN(0)));
    assert.ok(batchStateAccount.totalBaseTradedFp.gt(new BN(0)));

    // ----------------------------------------
    // 8) settleOrder (for the bid)
    // ----------------------------------------

    const [orderFillPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("order_fill"), orderBidPda.toBuffer()],
      programId
    );

    const txSettle = await pg.program.methods
      .settleOrder()
      .accounts({
        user: wallet.publicKey,
        market: marketPda,
        batchState: batchStatePda,
        order: orderBidPda,
        orderFill: orderFillPda,
        vaultBase: vaultBasePda,
        vaultQuote: vaultQuotePda,
        userBaseAta: userBaseAta.address,
        userQuoteAta: userQuoteAta.address,
        tokenProgram: splToken.TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .rpc();

    console.log("settleOrder tx:", txSettle);

    const orderFillAccount = await pg.program.account.orderFill.fetch(
      orderFillPda
    );

    assert.equal(orderFillAccount.order.toBase58(), orderBidPda.toBase58());
    assert.equal(
      orderFillAccount.batchId.toNumber(),
      batchStateAccount.batchId.toNumber()
    );
    assert.equal(orderFillAccount.claimed, true);

    console.log("OrderFill (bid):", {
      filledBase: orderFillAccount.filledBaseFp.toString(),
      filledQuote: orderFillAccount.filledQuoteFp.toString(),
      refundBase: orderFillAccount.refundBaseFp.toString(),
      refundQuote: orderFillAccount.refundQuoteFp.toString(),
    });
  });
});
