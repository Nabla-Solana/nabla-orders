//! Test-only stand-in for Jupiter (never deployed). It performs the TransferChecked calls its data lists, so the order
//! program's tests can play an honest route and every dishonest one. Data: a count, then per transfer the account indexes of
//! from, mint, to, authority and token program, the amount (u64), the decimals, and 1 to sign as the mock's vault PDA. Behind Jupiter's
//! route_v2 header (ROUTE_V2_HEADER bytes: its name, the input, the quoted output, the slippage the engine writes), as the engine
//! builds a fill from a route: the header passed over.
use anchor_lang::solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
    pubkey::Pubkey,
};

entrypoint!(process);

/// sha256("global:route_v2")[..8], and the header the engine reads its amounts from (it requires its name and 30 bytes past the 8th).
const ROUTE_V2: [u8; 8] = [187, 100, 250, 204, 49, 196, 175, 20];
const ROUTE_V2_HEADER: usize = 39;

fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let bump = [Pubkey::find_program_address(&[b"vault"], program_id).1];
    let vault: &[&[u8]] = &[b"vault", &bump];
    let data = if data.len() > ROUTE_V2_HEADER && data[..8] == ROUTE_V2 { &data[ROUTE_V2_HEADER..] } else { data };
    for t in data[1..].chunks(15).take(data[0] as usize) {
        let [from, mint, to, authority, program] = [t[0], t[1], t[2], t[3], t[4]].map(|i| &accounts[i as usize]);
        let mut ix = vec![12];
        ix.extend_from_slice(&t[5..14]);
        let metas = vec![AccountMeta::new(*from.key, false), AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new(*to.key, false), AccountMeta::new_readonly(*authority.key, true)];
        let signers: &[&[&[u8]]] = if t[14] == 1 { &[vault] } else { &[] };
        invoke_signed(&Instruction { program_id: *program.key, accounts: metas, data: ix },
            &[from.clone(), mint.clone(), to.clone(), authority.clone()], signers)?;
    }
    Ok(())
}
