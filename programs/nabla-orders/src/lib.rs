//! NABLA's order guard: users' limit buys, stop-losses and take-profits, filled by NABLA's engine through
//! Jupiter. The user's tokens stay in their own wallet under a spending permission (an SPL approval) to a PDA of this
//! program, one per (user, mint). A fill can only spend the order's exact amount, only through Jupiter, only into the user's
//! own account, for at least the minimum the user signed, and only with NABLA's referral fee paid. When to fire and which
//! route to take are the engine's; this program checks the outcome.
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::Instruction,
    program::{invoke, invoke_signed},
    system_instruction,
};

declare_id!("AAAtde5ZAg136H2RrNtMohS7k8AVprbGBBEqhT1S8uGA");

/// Jupiter v6: the one swap program a fill may call.
pub const JUPITER: Pubkey = pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
pub const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ATA_PROGRAM: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
/// Token-2022's own native mint: never a destination either (a route could sync stray lamports into it).
pub const NATIVE_2022: Pubkey = pubkey!("9pan9bMn5HatX4EJdBwg9VgCa7Uz5HL8N1m5D3NdXejP");
pub const USDC: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
/// Every order has SOL, USDC or USDT on one side; NABLA's fee is taken on that side (the input's when both are).
pub const QUOTES: [Pubkey; 3] = [WSOL, USDC, USDT];
/// The highest fee the config may set (1%): a stolen admin key cannot raise it past this.
pub const MAX_FEE_BPS: u16 = 100;
/// A dollar order: SOL on one side, its minimum in millionths of a dollar, judged at the fill by Pyth's SOL/USD. The top
/// bit of `min_den` marks it; no order before it sets that bit (their denominators are powers of ten up to 10^18), so the account
/// keeps its size and every earlier order reads as it did.
pub const USD: u64 = 1 << 63;
/// Pyth's receiver program, the owner of its verified price updates (PriceUpdateV2 accounts).
pub const PYTH_RECEIVER: Pubkey = pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
/// Pyth's own SOL/USD account (its push oracle's, shard 0), the only one read: it takes only a newer verified update, so a fill can
/// never be judged by an older price chosen within the age allowed, nor `fill_usd_v3`'s fence moved back.
pub const SOL_USD_ACCOUNT: Pubkey = pubkey!("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE");
/// sha256("account:PriceUpdateV2")[..8], as on chain.
const PRICE_UPDATE: [u8; 8] = [0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd];
/// Pyth's SOL/USD feed id.
const SOL_USD_FEED: [u8; 32] = [0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d];
/// A price older than this (seconds), or whose confidence is wider than 1% of it, is not used by `fill_usd` or `fill_usd_v2`: the fill
/// waits. (Pyth's sponsor pushes this account about every 50-55 s, so `fill_usd_v3` judges at the price sent instead.)
pub const PRICE_MAX_AGE: i64 = 30;
/// `fill_usd_v3` judges a dollar order at SOL's
/// dollar price the keeper sends, within BAND_BPS of Pyth's SOL/USD at most BACKSTOP_MAX_AGE old: Pyth bounds the price, not the time.
pub const BAND_BPS: u128 = 500;
pub const BACKSTOP_MAX_AGE: i64 = 300;

#[program]
pub mod nabla_orders {
    use super::*;

    /// Once, by the program's upgrade authority, who becomes the admin.
    pub fn init_config(ctx: Context<InitConfig>, keeper: Pubkey, referral: Pubkey, fee_bps: u16) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, OrderError::BadFee);
        let admin = ctx.accounts.admin.key();
        ctx.accounts.config.set_inner(Config { admin, keeper, referral, fee_bps, paused: false });
        Ok(())
    }

    /// The admin's: a new admin or keeper, the fee rate, a pause.
    pub fn set_config(ctx: Context<SetConfig>, config: Config) -> Result<()> {
        require!(config.fee_bps <= MAX_FEE_BPS, OrderError::BadFee);
        ctx.accounts.config.set_inner(config);
        Ok(())
    }

    /// The user's: an order spending `source` (their token account, under the permission given in the same transaction)
    /// into `destination` (their token account), or into their wallet as SOL when there is none.
    pub fn create_order(ctx: Context<CreateOrder>, id: u64, args: OrderArgs) -> Result<()> {
        create(ctx, id, args, 0)
    }

    /// `create_order`
    /// with a take-profit's own least, `take_min_num` over the same `min_den`, beside the stop-loss's (`min_num`): each side its own
    /// worst price, signed. Only a sale with a take-profit has one; 0 keeps the order's one least for both sides, as before.
    pub fn create_order_v2(ctx: Context<CreateOrder>, id: u64, args: OrderArgs, take_min_num: u64) -> Result<()> {
        create(ctx, id, args, take_min_num)
    }

    /// The user's, at any time: the order closes and its deposit comes back (`close = user`).
    pub fn cancel_order(_ctx: Context<CancelOrder>) -> Result<()> {
        Ok(())
    }

    /// The engine's. `route` is Jupiter's swap instruction data, its accounts following as remaining accounts with the
    /// authority as taker. The order's amount moves from the user's account into the authority's buffer, Jupiter swaps it
    /// into the authority's output buffer, and the outcome is checked before the user is paid. A direct fill names the
    /// user's own account as the output buffer: the route pays into it itself.
    pub fn fill<'info>(ctx: Context<'info, Fill<'info>>, route: Vec<u8>) -> Result<()> {
        let bump = ctx.bumps.authority;
        settle(ctx.accounts, ctx.remaining_accounts, bump, route, None, 0)
    }

    /// A dollar order's fill, `fill`'s own with Pyth's
    /// SOL/USD to judge its minimum by. `fill` refuses a dollar order and this a plain one, so neither is ever judged the other way.
    pub fn fill_usd<'info>(ctx: Context<'info, FillUsd<'info>>, route: Vec<u8>) -> Result<()> {
        let (bump, price) = (ctx.bumps.fill.authority, sol_usd(&ctx.accounts.sol_usd.to_account_info(), Clock::get()?.unix_timestamp, PRICE_MAX_AGE)?);
        settle(&mut ctx.accounts.fill, ctx.remaining_accounts, bump, route, Some(price), 0)
    }

    /// `fill` with the side the engine fires: 0 an order's one least (a buy, or a sale without a take-profit's own); 1 a sale
    /// at its stop-loss, judged by `min_num`; 2 a sale at its take-profit, judged by `take_min_num` when it has its own.
    pub fn fill_v2<'info>(ctx: Context<'info, Fill<'info>>, route: Vec<u8>, side: u8) -> Result<()> {
        let bump = ctx.bumps.authority;
        settle(ctx.accounts, ctx.remaining_accounts, bump, route, None, side)
    }

    /// `fill_usd` with the side the engine fires, as `fill_v2`.
    pub fn fill_usd_v2<'info>(ctx: Context<'info, FillUsd<'info>>, route: Vec<u8>, side: u8) -> Result<()> {
        let (bump, price) = (ctx.bumps.fill.authority, sol_usd(&ctx.accounts.sol_usd.to_account_info(), Clock::get()?.unix_timestamp, PRICE_MAX_AGE)?);
        settle(&mut ctx.accounts.fill, ctx.remaining_accounts, bump, route, Some(price), side)
    }

    /// A backstop that reads Pyth but acts on the price the keeper signs and sends with the fill, as long as it is within 5% of Pyth;
    /// the keeper's key is changeable by the admin. `fill_usd_v2` judged
    /// at SOL's dollar price the keeper sends (`sol_price`, millionths of a dollar a SOL: the price the app shows), within BAND_BPS of Pyth's
    /// SOL/USD (`shipped`). The keeper is the only signer a fill accepts (`Fill`), and only the admin changes it (`set_config`).
    pub fn fill_usd_v3<'info>(ctx: Context<'info, FillUsd<'info>>, route: Vec<u8>, side: u8, sol_price: u64) -> Result<()> {
        let (bump, price) = (ctx.bumps.fill.authority, shipped(&ctx.accounts.sol_usd.to_account_info(), Clock::get()?.unix_timestamp, sol_price)?);
        settle(&mut ctx.accounts.fill, ctx.remaining_accounts, bump, route, Some(price), side)
    }

    /// Anyone's, once per order: an order of the layout before `take_min_num` grown to today's, the new field 0 (its one
    /// least for both sides, as it was signed), `payer` paying the rent of the 8 bytes. One already grown is left as it is; any
    /// other account is refused.
    pub fn migrate_order(ctx: Context<MigrateOrder>) -> Result<()> {
        let order = ctx.accounts.order.to_account_info();
        let size = 8 + Order::INIT_SPACE;
        {
            let d = order.try_borrow_data()?;
            require!(d.len() >= 8 && &d[..8] == Order::DISCRIMINATOR, OrderError::BadOrder);
            if d.len() == size {
                return Ok(());
            }
            require!(d.len() == size - 8, OrderError::BadOrder);
        }
        let short = Rent::get()?.minimum_balance(size).saturating_sub(order.lamports());
        if short > 0 {
            let payer = ctx.accounts.payer.to_account_info();
            invoke(&system_instruction::transfer(payer.key, order.key, short), &[payer, order.clone(), ctx.accounts.system_program.to_account_info()])?;
        }
        order.resize(size)?;
        Ok(())
    }
}

/// `create_order` and `create_order_v2`: the order as the user signed it.
fn create(ctx: Context<CreateOrder>, id: u64, args: OrderArgs, take_min_num: u64) -> Result<()> {
    let user = ctx.accounts.user.key();
    let (input_mint, owner, _) = token_account(&ctx.accounts.source)?;
    require_keys_eq!(owner, user, OrderError::NotUsers);
    let (destination, output_mint) = match &ctx.accounts.destination {
        None => (Pubkey::default(), WSOL),
        Some(d) => {
            let (mint, owner, _) = token_account(d)?;
            require_keys_eq!(owner, user, OrderError::NotUsers);
            require!(mint != WSOL && mint != NATIVE_2022, OrderError::BadOrder); // SOL comes out into the wallet itself
            (d.key(), mint)
        }
    };
    require_keys_neq!(input_mint, output_mint, OrderError::BadOrder);
    require!(QUOTES.contains(&input_mint) || QUOTES.contains(&output_mint), OrderError::NoQuoteSide);
    // A plain order spends its amount; an exit created with a buy waits, empty, until the buy fills.
    require!((args.amount > 0) == (args.parent == Pubkey::default()), OrderError::BadOrder);
    require!(args.min_num > 0 && args.min_den > 0, OrderError::BadOrder);
    require!(args.min_den & USD == 0 || ((input_mint == WSOL || output_mint == WSOL) && args.min_den & !USD > 0), OrderError::BadOrder);
    require!(args.expiry > Clock::get()?.unix_timestamp, OrderError::Expired);
    // A take-profit's own least only on a sale with a take-profit.
    require!(take_min_num == 0 || (!QUOTES.contains(&input_mint) && args.upper > 0), OrderError::BadOrder);
    let source = ctx.accounts.source.key();
    ctx.accounts.order.set_inner(Order {
        user, id, source, input_mint, destination, output_mint, amount: args.amount, min_num: args.min_num,
        min_den: args.min_den, lower: args.lower, upper: args.upper, expiry: args.expiry, parent: args.parent, take_min_num,
    });
    Ok(())
}

/// `fill`, `fill_usd` and their `_v2` (`side`: see `fill_v2`): the order's amount moves from the user's account into the
/// authority's buffer, Jupiter swaps it into the authority's output buffer, and the outcome is checked before the user is paid.
/// `price`: for a dollar order, SOL's dollar price as (price, confidence, exponent): Pyth's (`fill_usd`, `fill_usd_v2`), or the
/// keeper's within Pyth's band (`fill_usd_v3`).
fn settle<'info>(fill: &mut Fill<'info>, remaining: &[AccountInfo<'info>], bump: u8, route: Vec<u8>, price: Option<(u128, u128, i32)>, side: u8) -> Result<()> {
    let a = &*fill;
    let order = &a.order;
    require!(!a.config.paused, OrderError::Paused);
    require!(order.amount > 0, OrderError::Waiting);
    let now = Clock::get()?.unix_timestamp;
    require!(now < order.expiry, OrderError::Expired);
    // A dollar order only with its price (fill_usd), a plain one only without (fill).
    require!((order.min_den & USD != 0) == price.is_some(), OrderError::BadOrder);
    require_keys_eq!(*a.input_mint.owner, a.input_token_program.key(), OrderError::WrongTokenProgram);
    require_keys_eq!(*a.output_mint.owner, a.output_token_program.key(), OrderError::WrongTokenProgram);
    let sol_out = order.output_mint == WSOL;
    require!(sol_out == a.destination.is_none(), OrderError::WrongDestination);
    if let Some(d) = &a.destination {
        require_keys_eq!(d.key(), order.destination, OrderError::WrongDestination);
    }
    let authority = a.authority.key();
    // Every token stays tradeable, taxed ones included: a token
    // taxed on every transfer (Token-2022's transfer fee) bought is paid into the user's own account by the route itself, taxed once as in
    // a swap: the engine names the destination as the output buffer. (A sale cannot: Jupiter takes only from an account its taker owns,
    // so a taxed token sold moves through the authority's buffer and is taxed twice; the tax it holds back there goes to the mint below.)
    let direct_out = a.destination.as_ref().is_some_and(|d| d.key() == a.output_buffer.key());
    let (mint, owner, in_pre) = token_account(&a.input_buffer)?;
    require!(mint == order.input_mint && owner == authority, OrderError::WrongBuffer);
    let (mint, owner, out_pre) = token_account(&a.output_buffer)?;
    require!(mint == order.output_mint && (owner == authority || direct_out), OrderError::WrongBuffer);
    let fee_on_input = QUOTES.contains(&order.input_mint);
    // The least judged by the side the engine fires. A sale signed with its take-profit's own least is filled only with its
    // side said; a buy only as itself (side 0); a side the order does not have is refused.
    let num = match side {
        0 => { require!(order.take_min_num == 0, OrderError::BadSide); order.min_num }
        1 => { require!(!fee_on_input && order.lower > 0, OrderError::BadSide); order.min_num }
        2 => {
            require!(!fee_on_input && order.upper > 0, OrderError::BadSide);
            if order.take_min_num > 0 { order.take_min_num } else { order.min_num }
        }
        _ => return err!(OrderError::BadSide),
    };
    let (fee_mint, fee_program) = if fee_on_input {
        (order.input_mint, a.input_token_program.key())
    } else {
        (order.output_mint, a.output_token_program.key())
    };
    let fee_account = Pubkey::find_program_address(&[a.config.referral.as_ref(), fee_program.as_ref(), fee_mint.as_ref()], &ATA_PROGRAM).0;
    require_keys_eq!(a.fee_account.key(), fee_account, OrderError::WrongFeeAccount);
    let fee_pre = token_account(&a.fee_account)?.2;
    let source_pre = token_account(&a.source)?.2;
    let (in_decimals, out_decimals) = (mint_decimals(&a.input_mint)?, mint_decimals(&a.output_mint)?);
    let bump = [bump];
    let seeds: &[&[u8]] = &[b"authority", order.user.as_ref(), order.input_mint.as_ref(), &bump];

    // The order's exact amount, from the user's account (the authority is its delegate) into the authority's buffer.
    transfer_checked(&a.input_token_program, &a.source, &a.input_mint, &a.input_buffer, &a.authority, order.amount, in_decimals, seeds)?;
    // A fee on the input side (a buy paid in SOL, USDC or USDT) is this program's own transfer into NABLA's referral account:
    // Jupiter takes an integrator fee from the output only. The route then swaps the rest.
    let bps = a.config.fee_bps as u128;
    if fee_on_input {
        let input_fee = (order.amount as u128 * bps / 10_000) as u64;
        if input_fee > 0 {
            transfer_checked(&a.input_token_program, &a.input_buffer, &a.input_mint, &a.fee_account, &a.authority, input_fee, in_decimals, seeds)?;
        }
    }
    // Jupiter's route, the authority signing as its taker; the keeper, when the route has it pay a venue's rent, signs as itself.
    let accounts = remaining.iter()
        .map(|x| AccountMeta { pubkey: *x.key, is_signer: *x.key == authority || x.is_signer, is_writable: x.is_writable })
        .collect();
    invoke_signed(&Instruction { program_id: JUPITER, accounts, data: route }, remaining, &[seeds])?;

    // The outcome: exactly the order's amount left the user and was swapped, at least the minimum came out, NABLA was paid.
    require!(source_pre.checked_sub(token_account(&a.source)?.2) == Some(order.amount), OrderError::WrongDebit);
    require!(token_account(&a.input_buffer)?.2 == in_pre, OrderError::RouteMismatch);
    let out = token_account(&a.output_buffer)?.2.checked_sub(out_pre).ok_or(OrderError::BelowMinimum)?;
    let fee = token_account(&a.fee_account)?.2.checked_sub(fee_pre).ok_or(OrderError::FeeShort)? as u128;
    // Jupiter's floor(gross × bps / 10,000) from the output; an input-side fee is this program's own transfer above.
    require!(fee_on_input || fee * (10_000 - bps) + 10_000 > out as u128 * bps, OrderError::FeeShort);

    // Pay the user: SOL into their wallet, a token into their account (a direct fill's route paid it there). Buffers that began empty go
    // back to the keeper. The user's minimum is judged on what reaches their account (a token's own transfer fee is taken on the way).
    let received = if sol_out {
        close_account(&a.output_token_program, &a.output_buffer, &a.keeper, &a.authority, seeds)?; // every lamport to the keeper
        invoke(&system_instruction::transfer(&a.keeper.key(), &a.user.key(), out), &[a.keeper.to_account_info(), a.user.to_account_info()])?;
        out
    } else if direct_out {
        out
    } else {
        let d = a.destination.as_ref().ok_or(OrderError::WrongDestination)?;
        let before = token_account(d)?.2;
        transfer_checked(&a.output_token_program, &a.output_buffer, &a.output_mint, d, &a.authority, out, out_decimals, seeds)?;
        if out_pre == 0 {
            close_account(&a.output_token_program, &a.output_buffer, &a.keeper, &a.authority, seeds)?;
        }
        token_account(d)?.2.checked_sub(before).ok_or(OrderError::BelowMinimum)?
    };
    let minimum = minimum_out(order, num, price)?;
    require!(received as u128 >= minimum, OrderError::BelowMinimum);
    if in_pre == 0 || order.input_mint == WSOL {
        // Token-2022 will not close an account holding withheld tax: a taxed token's is moved to its mint first (HarvestWithheldTokensToMint,
        // anyone's to call; the engine marks the mint writable for it), so the buffer closes back to the keeper as any other.
        if withheld(&a.input_buffer)? > 0 {
            let accounts = vec![AccountMeta::new(a.input_mint.key(), false), AccountMeta::new(a.input_buffer.key(), false)];
            invoke(&Instruction { program_id: TOKEN_2022, accounts, data: vec![26, 4] }, &[a.input_mint.to_account_info(), a.input_buffer.to_account_info()])?;
        }
        close_account(&a.input_token_program, &a.input_buffer, &a.keeper, &a.authority, seeds)?;
    }

    // A bracket's exit gets exactly what the buy delivered to the user.
    let (key, user, destination) = (order.key(), order.user, order.destination);
    if let Some(child) = fill.child.as_mut() {
        require!(child.parent == key && child.user == user && child.source == destination && child.amount == 0, OrderError::WrongChild);
        child.amount = received;
    }
    // What the user received and NABLA's fee, for the engine's record of the fill.
    msg!("filled {} {}", received, fee);
    Ok(())
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// The engine's key: the only signer a fill accepts.
    pub keeper: Pubkey,
    /// NABLA's Jupiter referral account; its token accounts receive the fee.
    pub referral: Pubkey,
    pub fee_bps: u16,
    pub paused: bool,
}

#[account]
#[derive(InitSpace)]
pub struct Order {
    pub user: Pubkey,
    pub id: u64,
    /// The user's token account a fill spends from (their permission's account).
    pub source: Pubkey,
    pub input_mint: Pubkey,
    /// The user's token account a fill pays into; the default key when the output is SOL into the wallet.
    pub destination: Pubkey,
    pub output_mint: Pubkey,
    /// The exact amount a fill spends; 0 while a bracket's exit waits for its buy.
    pub amount: u64,
    /// The least a fill may pay out: amount × min_num / min_den.
    pub min_num: u64,
    pub min_den: u64,
    /// The trigger prices the user signed, recorded for the engine (not enforced here).
    pub lower: u64,
    pub upper: u64,
    pub expiry: i64,
    /// For a bracket's exit, its buy; the default key otherwise.
    pub parent: Pubkey,
    /// A sale's take-profit's own least (`amount × take_min_num / min_den`), beside `min_num`, its stop-loss's; 0 for none:
    /// `min_num` judges both sides, as for every order before it (each migrated with 0).
    pub take_min_num: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct OrderArgs {
    pub amount: u64,
    pub min_num: u64,
    pub min_den: u64,
    pub lower: u64,
    pub upper: u64,
    pub expiry: i64,
    pub parent: Pubkey,
}

#[derive(Accounts)]
pub struct InitConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()))]
    pub program: Program<'info, crate::program::NablaOrders>,
    #[account(constraint = program_data.upgrade_authority_address == Some(admin.key()) @ OrderError::NotAdmin)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump, has_one = admin @ OrderError::NotAdmin)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct CreateOrder<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(init, payer = user, space = 8 + Order::INIT_SPACE, seeds = [b"order", user.key().as_ref(), &id.to_le_bytes()], bump)]
    pub order: Account<'info, Order>,
    /// CHECK: read in the handler: a token account the user owns.
    pub source: UncheckedAccount<'info>,
    /// CHECK: read in the handler: a token account the user owns; none for SOL into the wallet.
    pub destination: Option<UncheckedAccount<'info>>,
    pub system_program: Program<'info, System>,
}

/// An order of the earlier layout and whoever pays its 8 bytes' rent.
#[derive(Accounts)]
pub struct MigrateOrder<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: this program's; an order of the earlier layout, its discriminator and size checked in the handler.
    #[account(mut, owner = crate::ID)]
    pub order: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelOrder<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, close = user, has_one = user)]
    pub order: Account<'info, Order>,
}

#[derive(Accounts)]
pub struct Fill<'info> {
    #[account(seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    #[account(mut, address = config.keeper @ OrderError::NotKeeper)]
    pub keeper: Signer<'info>,
    #[account(mut, close = user, has_one = user, has_one = source, has_one = input_mint, has_one = output_mint)]
    pub order: Account<'info, Order>,
    /// CHECK: the order's user (has_one): gets the deposit back, and the SOL when the output is SOL.
    #[account(mut)]
    pub user: UncheckedAccount<'info>,
    /// CHECK: the user's permission holder for the input mint; it signs only through this program.
    #[account(seeds = [b"authority", user.key().as_ref(), input_mint.key().as_ref()], bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: the order's source (has_one).
    #[account(mut)]
    pub source: UncheckedAccount<'info>,
    /// CHECK: the order's destination, checked in the handler; none when the output is SOL.
    #[account(mut)]
    pub destination: Option<UncheckedAccount<'info>>,
    /// CHECK: has_one; read for its decimals; writable when a taxed token's withheld tax is moved to it.
    pub input_mint: UncheckedAccount<'info>,
    /// CHECK: has_one; read for its decimals.
    pub output_mint: UncheckedAccount<'info>,
    /// CHECK: the authority's token account of the input mint, checked in the handler.
    #[account(mut)]
    pub input_buffer: UncheckedAccount<'info>,
    /// CHECK: the authority's token account of the output mint, or the destination itself for a direct fill; checked in the handler.
    #[account(mut)]
    pub output_buffer: UncheckedAccount<'info>,
    /// CHECK: NABLA's referral token account for the fee mint, derived in the handler.
    #[account(mut)]
    pub fee_account: UncheckedAccount<'info>,
    /// CHECK: the input mint's owner, checked in the handler.
    pub input_token_program: UncheckedAccount<'info>,
    /// CHECK: the output mint's owner, checked in the handler.
    pub output_token_program: UncheckedAccount<'info>,
    /// CHECK: Jupiter v6.
    #[account(address = JUPITER)]
    pub jupiter: UncheckedAccount<'info>,
    /// A bracket's exit, armed with what this buy delivers.
    #[account(mut)]
    pub child: Option<Account<'info, Order>>,
    pub system_program: Program<'info, System>,
}

/// `fill`'s accounts, then Pyth's SOL/USD price update (`fill_usd_v3`'s too, Pyth its backstop).
#[derive(Accounts)]
pub struct FillUsd<'info> {
    pub fill: Fill<'info>,
    /// CHECK: Pyth's SOL/USD (a PriceUpdateV2): its owner, verification, feed, age and confidence checked in the handler.
    pub sol_usd: UncheckedAccount<'info>,
}

#[error_code]
pub enum OrderError {
    #[msg("The fee rate is out of range")]
    BadFee,
    #[msg("Only the admin may do this")]
    NotAdmin,
    #[msg("Only NABLA's engine may fill")]
    NotKeeper,
    #[msg("Fills are paused")]
    Paused,
    #[msg("This exit waits for its buy")]
    Waiting,
    #[msg("The order has expired")]
    Expired,
    #[msg("Not a token account")]
    NotTokenAccount,
    #[msg("Not a mint")]
    NotMint,
    #[msg("The account is not the user's")]
    NotUsers,
    #[msg("One side must be SOL, USDC or USDT")]
    NoQuoteSide,
    #[msg("Invalid order")]
    BadOrder,
    #[msg("Wrong buffer account")]
    WrongBuffer,
    #[msg("Wrong fee account")]
    WrongFeeAccount,
    #[msg("Wrong token program")]
    WrongTokenProgram,
    #[msg("Wrong destination")]
    WrongDestination,
    #[msg("The source lost more or less than the order's amount")]
    WrongDebit,
    #[msg("The route did not swap exactly the order's amount")]
    RouteMismatch,
    #[msg("The output is below the user's minimum")]
    BelowMinimum,
    #[msg("NABLA's fee was not paid")]
    FeeShort,
    #[msg("Wrong exit order")]
    WrongChild,
    #[msg("No fresh, narrow SOL/USD price from Pyth")]
    BadPrice,
    #[msg("Not a side this order has")]
    BadSide,
    #[msg("The SOL price sent is more than 5% off Pyth's")]
    PriceOff,
}

/// The least a fill pays out: `amount × num / min_den` (`num` the side's: `min_num`, or `take_min_num`), in the output's units. A dollar order's is in millionths of a dollar:
/// a sale's floor turned into lamports at the low edge of SOL's dollar price (`price`), or a buy's SOL turned into millionths of a dollar
/// at its high edge and then into tokens at the user's rate; each edge the one that favours the user (Pyth's confidence; the price the
/// keeper sends has none). Any overflow refuses the fill.
fn minimum_out(order: &Order, num: u64, price: Option<(u128, u128, i32)>) -> Result<u128> {
    let (num, den) = (num as u128, (order.min_den & !USD) as u128);
    // Every division rounded up, so a fill never pays even a unit under the user's terms.
    let up = |a: u128, b: u128| a.div_ceil(b);
    if order.min_den & USD == 0 {
        return Ok(up(order.amount as u128 * num, den));
    }
    let (p, c, expo) = price.ok_or(OrderError::BadPrice)?;
    // Dollars per SOL are p × 10^expo, lamports per SOL 10^9, millionths per dollar 10^6: a lamport is p × 10^(expo - 3) millionths.
    let shift = expo - 3;
    require!((-30..=30).contains(&shift), OrderError::BadPrice);
    let ten = 10u128.pow(shift.unsigned_abs());
    let over = || error!(OrderError::BadOrder);
    let amount = order.amount as u128;
    Ok(if order.output_mint == WSOL {
        let usd = up(amount.checked_mul(num).ok_or_else(over)?, den);
        let low = p - c;
        if shift < 0 { up(usd.checked_mul(ten).ok_or_else(over)?, low) } else { up(usd, low.checked_mul(ten).ok_or_else(over)?) }
    } else {
        let paid = amount.checked_mul(p + c).ok_or_else(over)?;
        let usd = if shift < 0 { up(paid, ten) } else { paid.checked_mul(ten).ok_or_else(over)? };
        up(usd.checked_mul(num).ok_or_else(over)?, den)
    })
}

/// Pyth's SOL/USD: its own account (SOL_USD_ACCOUNT), a PriceUpdateV2 its receiver owns, fully verified, of the SOL/USD feed, at most
/// `max_age` old, its confidence
/// within 1% of its price. Its layout: discriminator, write authority (8..40), verification level (40: 1 is full), then the message:
/// feed id (41..73), price (73), confidence (81), exponent (89), publish time (93). Returns (price, confidence, exponent).
fn sol_usd(account: &AccountInfo, now: i64, max_age: i64) -> Result<(u128, u128, i32)> {
    require_keys_eq!(*account.key, SOL_USD_ACCOUNT, OrderError::BadPrice);
    require_keys_eq!(*account.owner, PYTH_RECEIVER, OrderError::BadPrice);
    let d = account.try_borrow_data()?;
    require!(d.len() >= 101 && d[..8] == PRICE_UPDATE && d[40] == 1 && d[41..73] == SOL_USD_FEED, OrderError::BadPrice);
    let int = |at: usize| i64::from_le_bytes(d[at..at + 8].try_into().unwrap());
    let (price, conf, expo, published) = (int(73), u64::from_le_bytes(d[81..89].try_into().unwrap()), i32::from_le_bytes(d[89..93].try_into().unwrap()), int(93));
    require!(price > 0 && conf as u128 * 100 <= price as u128 && now - published <= max_age, OrderError::BadPrice);
    Ok((price as u128, conf as u128, expo))
}

/// SOL's dollar price as the keeper sent it (millionths of a dollar a SOL), used only within BAND_BPS of Pyth's SOL/USD, read with
/// sol_usd's checks at most BACKSTOP_MAX_AGE old: whatever key signs as the keeper, the price it sends is held to Pyth's. Returned in
/// minimum_out's terms: the price as sent, no confidence, exponent -6.
fn shipped(account: &AccountInfo, now: i64, sol_price: u64) -> Result<(u128, u128, i32)> {
    let (p, _, expo) = sol_usd(account, now, BACKSTOP_MAX_AGE)?;
    // Pyth's price in millionths of a dollar a SOL: p × 10^(expo + 6).
    let shift = expo + 6;
    require!((-30..=30).contains(&shift), OrderError::BadPrice);
    let ten = 10u128.pow(shift.unsigned_abs());
    let pyth = if shift < 0 { p / ten } else { p.checked_mul(ten).ok_or(OrderError::BadPrice)? };
    let (sent, lo, hi) = (sol_price as u128 * 10_000, pyth.checked_mul(10_000 - BAND_BPS), pyth.checked_mul(10_000 + BAND_BPS));
    require!(sol_price > 0 && lo.zip(hi).is_some_and(|(lo, hi)| (lo..=hi).contains(&sent)), OrderError::PriceOff);
    Ok((sol_price as u128, 0, -6))
}

/// A token account's mint, owner and amount (SPL Token or Token-2022; an extended account carries type 2 at byte 165).
fn token_account(account: &AccountInfo) -> Result<(Pubkey, Pubkey, u64)> {
    require!(*account.owner == TOKEN || *account.owner == TOKEN_2022, OrderError::NotTokenAccount);
    let d = account.try_borrow_data()?;
    require!(d.len() >= 165 && d[108] == 1 && (d.len() == 165 || d[165] == 2), OrderError::NotTokenAccount);
    let key = |at: usize| Pubkey::new_from_array(d[at..at + 32].try_into().unwrap());
    Ok((key(0), key(32), u64::from_le_bytes(d[64..72].try_into().unwrap())))
}

/// The transfer tax a Token-2022 account holds back (its TransferFeeAmount extension, type 2: the u64 withheld), 0 for any other.
fn withheld(account: &AccountInfo) -> Result<u64> {
    let d = account.try_borrow_data()?;
    let mut at = 166;
    while *account.owner == TOKEN_2022 && at + 4 <= d.len() {
        let (kind, len) = (u16::from_le_bytes([d[at], d[at + 1]]), u16::from_le_bytes([d[at + 2], d[at + 3]]) as usize);
        if kind == 2 && len == 8 && at + 12 <= d.len() {
            return Ok(u64::from_le_bytes(d[at + 4..at + 12].try_into().unwrap()));
        }
        at += 4 + len;
    }
    Ok(0)
}

/// A mint's decimals (the order's own mint, by has_one; TransferChecked checks them again).
fn mint_decimals(account: &AccountInfo) -> Result<u8> {
    require!(*account.owner == TOKEN || *account.owner == TOKEN_2022, OrderError::NotMint);
    let d = account.try_borrow_data()?;
    require!(d.len() >= 82, OrderError::NotMint);
    Ok(d[44])
}

#[allow(clippy::too_many_arguments)]
fn transfer_checked<'info>(program: &AccountInfo<'info>, from: &AccountInfo<'info>, mint: &AccountInfo<'info>, to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>, amount: u64, decimals: u8, seeds: &[&[u8]]) -> Result<()> {
    let mut data = vec![12]; // TransferChecked, the same in SPL Token and Token-2022
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let accounts = vec![AccountMeta::new(*from.key, false), AccountMeta::new_readonly(*mint.key, false),
        AccountMeta::new(*to.key, false), AccountMeta::new_readonly(*authority.key, true)];
    invoke_signed(&Instruction { program_id: *program.key, accounts, data }, &[from.clone(), mint.clone(), to.clone(), authority.clone()], &[seeds])?;
    Ok(())
}

fn close_account<'info>(program: &AccountInfo<'info>, account: &AccountInfo<'info>, destination: &AccountInfo<'info>,
    owner: &AccountInfo<'info>, seeds: &[&[u8]]) -> Result<()> {
    let accounts = vec![AccountMeta::new(*account.key, false), AccountMeta::new(*destination.key, false), AccountMeta::new_readonly(*owner.key, true)];
    invoke_signed(&Instruction { program_id: *program.key, accounts, data: vec![9] }, &[account.clone(), destination.clone(), owner.clone()], &[seeds])?;
    Ok(())
}
