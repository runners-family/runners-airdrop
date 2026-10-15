//! RUNNERS airdrop claims.
//!
//! One distributor holds a Token-2022 token's entire supply and a fixed Merkle root of (wallet, amount) entitlements.
//! Each wallet claims its amount once, before the deadline, into a token account it owns; the transfer fee is withheld
//! from what it receives. After the deadline anyone can burn what is left in the vault.
//!
//! There is deliberately no admin: no instruction changes the root, the total, the deadline or where tokens go, and a
//! distributor can only be created over a vault that already holds every token of a mint that can no longer be minted,
//! frozen or moved by a delegate (plan 010). The mint may keep a transfer-fee config authority (the owner wants to be
//! able to lower or remove the fee); the fee in effect when the list is locked is recorded, and a claim is refused while
//! its fee would be higher, so raising the fee can delay claims but never take from them.
// solana-program 2.3 marks its system_instruction/system_program re-exports deprecated in favour of separate crates;
// they are identical, and the build stays on the burn program's dependency set.
#![allow(deprecated)]
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    hash::hashv,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_option::COption,
    pubkey,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::Sysvar,
};
use spl_token_2022::{
    extension::{transfer_fee::TransferFeeConfig, BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    state::{Account, AccountState, Mint},
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub const DISTRIBUTOR_SEED: &[u8] = b"airdrop";
pub const CLAIM_SEED: &[u8] = b"claim";
pub const DISTRIBUTOR_TAG: &[u8; 8] = b"RUNAIR02";
pub const CLAIM_TAG: &[u8; 8] = b"RUNCLM01";
pub const LEAF_DOMAIN: &[u8] = b"RUNNERS:airdrop:leaf:v1";
pub const NODE_DOMAIN: &[u8] = b"RUNNERS:airdrop:node:v1";
pub const MAX_PROOF: usize = 32;
/// The latest deadline initialization accepts, so a unit mistake (milliseconds for seconds) cannot make the unclaimed
/// rest unburnable.
pub const MAX_CLAIM_WINDOW_SECONDS: i64 = 400 * 24 * 60 * 60;
/// tag | mint | creator | root | total u64 | deadline i64 | claimed u64 | claims u64 | burned u64 |
/// fee basis points u16 | fee maximum u64 | bump
pub const DISTRIBUTOR_SIZE: usize = 8 + 32 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 2 + 8 + 1;
/// tag | amount u64
pub const CLAIM_SIZE: usize = 8 + 8;

/// Data `[0] ‖ root (32) ‖ total u64 ‖ deadline i64` (little-endian). Accounts: creator (signer, writable),
/// distributor (writable), mint, vault, Token-2022, system program.
pub const INITIALIZE: u8 = 0;
/// Data `[1] ‖ amount u64 ‖ proof length u8 ‖ proof (32 each)`. Accounts: claimant (signer, writable),
/// distributor (writable), claim marker (writable), vault (writable), destination (writable), mint, Token-2022,
/// system program.
pub const CLAIM: u8 = 1;
/// Data `[2]`. Accounts: distributor (writable), vault (writable), mint (writable), Token-2022. Anyone may call it
/// after the deadline.
pub const BURN_UNCLAIMED: u8 = 2;

const ASSOCIATED_TOKEN_PROGRAM: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AirdropError {
    InvalidInstruction = 0,
    InvalidTokenProgram,
    InvalidSystemProgram,
    MissingSignature,
    InvalidDistributor,
    InvalidMint,
    UnsupportedExtension,
    MintNotFixed,
    SupplyMismatch,
    InvalidVault,
    InvalidDestination,
    InvalidClaimMarker,
    AlreadyInitialized,
    AlreadyClaimed,
    InvalidProof,
    InvalidAmount,
    InvalidDeadline,
    ClaimsClosed,
    ClaimsOpen,
    Overflow,
    FeeRaised,
    FeeChangePending,
}
impl From<AirdropError> for ProgramError {
    fn from(value: AirdropError) -> Self {
        Self::Custom(value as u32)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Distributor {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub root: [u8; 32],
    pub total: u64,
    pub deadline: i64,
    pub claimed: u64,
    pub claims: u64,
    pub burned: u64,
    /// The transfer fee in effect when the list was locked (zero without a fee extension).
    pub fee_basis_points: u16,
    pub fee_maximum: u64,
    pub bump: u8,
}
impl Distributor {
    pub fn unpack(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() != DISTRIBUTOR_SIZE || &data[..8] != DISTRIBUTOR_TAG {
            return Err(AirdropError::InvalidDistributor.into());
        }
        let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
        Ok(Self {
            mint: key(8),
            creator: key(40),
            root: data[72..104].try_into().unwrap(),
            total: u64_at(104),
            deadline: i64::from_le_bytes(data[112..120].try_into().unwrap()),
            claimed: u64_at(120),
            claims: u64_at(128),
            burned: u64_at(136),
            fee_basis_points: u16::from_le_bytes(data[144..146].try_into().unwrap()),
            fee_maximum: u64_at(146),
            bump: data[154],
        })
    }
    pub fn pack(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(DISTRIBUTOR_TAG);
        data[8..40].copy_from_slice(self.mint.as_ref());
        data[40..72].copy_from_slice(self.creator.as_ref());
        data[72..104].copy_from_slice(&self.root);
        data[104..112].copy_from_slice(&self.total.to_le_bytes());
        data[112..120].copy_from_slice(&self.deadline.to_le_bytes());
        data[120..128].copy_from_slice(&self.claimed.to_le_bytes());
        data[128..136].copy_from_slice(&self.claims.to_le_bytes());
        data[136..144].copy_from_slice(&self.burned.to_le_bytes());
        data[144..146].copy_from_slice(&self.fee_basis_points.to_le_bytes());
        data[146..154].copy_from_slice(&self.fee_maximum.to_le_bytes());
        data[154] = self.bump;
    }
}

/// The distributor of `mint` created by `creator`.
pub fn distributor_address(program_id: &Pubkey, mint: &Pubkey, creator: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[DISTRIBUTOR_SEED, mint.as_ref(), creator.as_ref()], program_id)
}
/// The account whose existence records that `claimant` has claimed from `distributor`.
pub fn claim_address(program_id: &Pubkey, distributor: &Pubkey, claimant: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CLAIM_SEED, distributor.as_ref(), claimant.as_ref()], program_id)
}
/// The vault: the distributor's associated Token-2022 account for the mint.
pub fn vault_address(distributor: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[distributor.as_ref(), spl_token_2022::id().as_ref(), mint.as_ref()], &ASSOCIATED_TOKEN_PROGRAM).0
}
pub fn leaf(distributor: &Pubkey, claimant: &Pubkey, amount: u64) -> [u8; 32] {
    hashv(&[LEAF_DOMAIN, distributor.as_ref(), claimant.as_ref(), &amount.to_le_bytes()]).to_bytes()
}
/// Parent of two nodes, hashed as a sorted pair so a proof needs no left/right flags.
pub fn node(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let (left, right) = if a <= b { (a, b) } else { (b, a) };
    hashv(&[NODE_DOMAIN, left, right]).to_bytes()
}
pub fn verify(proof: &[[u8; 32]], root: &[u8; 32], leaf: [u8; 32]) -> bool {
    proof.iter().fold(leaf, |hash, sibling| node(&hash, sibling)) == *root
}
/// Token-2022's transfer fee on `amount`: the basis points rounded up, capped at `maximum`.
pub fn fee_for(amount: u64, basis_points: u16, maximum: u64) -> Option<u64> {
    if basis_points == 0 || amount == 0 {
        return Some(0);
    }
    let raw = (amount as u128).checked_mul(basis_points as u128)?.checked_add(9_999)? / 10_000;
    Some(u64::try_from(raw).ok()?.min(maximum))
}

pub fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    match data.first() {
        Some(&INITIALIZE) => initialize(program_id, accounts, &data[1..]),
        Some(&CLAIM) => claim(program_id, accounts, &data[1..]),
        Some(&BURN_UNCLAIMED) if data.len() == 1 => burn_unclaimed(program_id, accounts),
        _ => Err(AirdropError::InvalidInstruction.into()),
    }
}

fn initialize(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let accounts = &mut accounts.iter();
    let creator = next_account_info(accounts)?;
    let distributor = next_account_info(accounts)?;
    let mint = next_account_info(accounts)?;
    let vault = next_account_info(accounts)?;
    let token_program = next_account_info(accounts)?;
    let system = next_account_info(accounts)?;
    if data.len() != 48 {
        return Err(AirdropError::InvalidInstruction.into());
    }
    let root: [u8; 32] = data[..32].try_into().unwrap();
    let total = u64::from_le_bytes(data[32..40].try_into().unwrap());
    let deadline = i64::from_le_bytes(data[40..48].try_into().unwrap());
    if !creator.is_signer {
        return Err(AirdropError::MissingSignature.into());
    }
    check_programs(token_program, Some(system))?;
    if total == 0 {
        return Err(AirdropError::InvalidAmount.into());
    }
    let clock = Clock::get()?;
    if deadline <= clock.unix_timestamp || deadline > clock.unix_timestamp.saturating_add(MAX_CLAIM_WINDOW_SECONDS) {
        return Err(AirdropError::InvalidDeadline.into());
    }
    let (expected, bump) = distributor_address(program_id, mint.key, creator.key);
    if distributor.key != &expected {
        return Err(AirdropError::InvalidDistributor.into());
    }
    if mint.owner != token_program.key {
        return Err(AirdropError::InvalidMint.into());
    }
    let (fee_basis_points, fee_maximum) = {
        let bytes = mint.try_borrow_data()?;
        let state = StateWithExtensions::<Mint>::unpack(&bytes)?;
        // A permanent delegate could empty the vault and a transfer hook could block claims; the fee and the
        // metadata extensions are all a fixed-supply launch token needs.
        for extension in state.get_extension_types()? {
            if !matches!(extension, ExtensionType::TransferFeeConfig | ExtensionType::MetadataPointer | ExtensionType::TokenMetadata) {
                return Err(AirdropError::UnsupportedExtension.into());
            }
        }
        if state.base.mint_authority != COption::None || state.base.freeze_authority != COption::None {
            return Err(AirdropError::MintNotFixed.into());
        }
        if state.base.supply != total {
            return Err(AirdropError::SupplyMismatch.into());
        }
        match state.get_extension::<TransferFeeConfig>() {
            Ok(config) => {
                // A fee change still pending would take effect after the list is locked without being the recorded
                // fee; lock only once the mint's latest fee is the one in effect.
                if u64::from(config.newer_transfer_fee.epoch) > clock.epoch {
                    return Err(AirdropError::FeeChangePending.into());
                }
                let fee = config.get_epoch_fee(clock.epoch);
                (u16::from(fee.transfer_fee_basis_points), u64::from(fee.maximum_fee))
            }
            Err(_) => (0, 0),
        }
    };
    if vault.key != &vault_address(distributor.key, mint.key) || vault.owner != token_program.key {
        return Err(AirdropError::InvalidVault.into());
    }
    {
        let bytes = vault.try_borrow_data()?;
        let state = StateWithExtensions::<Account>::unpack(&bytes)?;
        if state.base.mint != *mint.key
            || state.base.owner != *distributor.key
            || state.base.delegate != COption::None
            || state.base.close_authority != COption::None
            || state.base.state != AccountState::Initialized
            || state.base.amount != total
        {
            return Err(AirdropError::InvalidVault.into());
        }
    }
    let bump_seed = [bump];
    create_owned(creator, distributor, system, program_id, DISTRIBUTOR_SIZE,
        &[DISTRIBUTOR_SEED, mint.key.as_ref(), creator.key.as_ref(), &bump_seed])?;
    Distributor { mint: *mint.key, creator: *creator.key, root, total, deadline, claimed: 0, claims: 0, burned: 0,
        fee_basis_points, fee_maximum, bump }
        .pack(&mut distributor.try_borrow_mut_data()?);
    msg!("airdrop: total {} deadline {} fee {} bps", total, deadline, fee_basis_points);
    Ok(())
}

fn claim(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let accounts = &mut accounts.iter();
    let claimant = next_account_info(accounts)?;
    let distributor = next_account_info(accounts)?;
    let marker = next_account_info(accounts)?;
    let vault = next_account_info(accounts)?;
    let destination = next_account_info(accounts)?;
    let mint = next_account_info(accounts)?;
    let token_program = next_account_info(accounts)?;
    let system = next_account_info(accounts)?;
    if data.len() < 9 {
        return Err(AirdropError::InvalidInstruction.into());
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    let levels = data[8] as usize;
    if levels > MAX_PROOF || data.len() != 9 + 32 * levels {
        return Err(AirdropError::InvalidInstruction.into());
    }
    let proof: Vec<[u8; 32]> = data[9..].chunks_exact(32).map(|chunk| chunk.try_into().unwrap()).collect();
    if !claimant.is_signer {
        return Err(AirdropError::MissingSignature.into());
    }
    check_programs(token_program, Some(system))?;
    let mut state = load(program_id, distributor)?;
    if mint.key != &state.mint {
        return Err(AirdropError::InvalidMint.into());
    }
    let clock = Clock::get()?;
    if clock.unix_timestamp >= state.deadline {
        return Err(AirdropError::ClaimsClosed.into());
    }
    if amount == 0 {
        return Err(AirdropError::InvalidAmount.into());
    }
    if !verify(&proof, &state.root, leaf(distributor.key, claimant.key, amount)) {
        return Err(AirdropError::InvalidProof.into());
    }
    let (expected, marker_bump) = claim_address(program_id, distributor.key, claimant.key);
    if marker.key != &expected {
        return Err(AirdropError::InvalidClaimMarker.into());
    }
    if marker.owner == program_id {
        return Err(AirdropError::AlreadyClaimed.into());
    }
    if vault.key != &vault_address(distributor.key, mint.key) {
        return Err(AirdropError::InvalidVault.into());
    }
    if destination.owner != token_program.key {
        return Err(AirdropError::InvalidDestination.into());
    }
    {
        let bytes = destination.try_borrow_data()?;
        let account = StateWithExtensions::<Account>::unpack(&bytes)?;
        // Tokens go only to an account the claimant owns.
        if account.base.mint != *mint.key || account.base.owner != *claimant.key {
            return Err(AirdropError::InvalidDestination.into());
        }
    }
    state.claimed = state.claimed.checked_add(amount).ok_or(AirdropError::Overflow)?;
    state.claims = state.claims.checked_add(1).ok_or(AirdropError::Overflow)?;
    if state.claimed > state.total {
        return Err(AirdropError::InvalidAmount.into());
    }
    let marker_seed = [marker_bump];
    create_owned(claimant, marker, system, program_id, CLAIM_SIZE,
        &[CLAIM_SEED, distributor.key.as_ref(), claimant.key.as_ref(), &marker_seed])?;
    {
        let mut bytes = marker.try_borrow_mut_data()?;
        bytes[..8].copy_from_slice(CLAIM_TAG);
        bytes[8..16].copy_from_slice(&amount.to_le_bytes());
    }
    // The fee this transfer will pay (Token-2022 applies the fee of the current epoch) may not exceed the fee recorded
    // when the list was locked: a raised fee delays claims until it is lowered again, but never takes from them.
    let (decimals, fee_now) = {
        let bytes = mint.try_borrow_data()?;
        let mint_state = StateWithExtensions::<Mint>::unpack(&bytes)?;
        let fee = match mint_state.get_extension::<TransferFeeConfig>() {
            Ok(config) => config.get_epoch_fee(clock.epoch).calculate_fee(amount).ok_or(AirdropError::Overflow)?,
            Err(_) => 0,
        };
        (mint_state.base.decimals, fee)
    };
    if fee_now > fee_for(amount, state.fee_basis_points, state.fee_maximum).ok_or(AirdropError::Overflow)? {
        return Err(AirdropError::FeeRaised.into());
    }
    let bump_seed = [state.bump];
    invoke_signed(
        &spl_token_2022::instruction::transfer_checked(token_program.key, vault.key, mint.key, destination.key,
            distributor.key, &[], amount, decimals)?,
        &[vault.clone(), mint.clone(), destination.clone(), distributor.clone(), token_program.clone()],
        &[&[DISTRIBUTOR_SEED, state.mint.as_ref(), state.creator.as_ref(), &bump_seed]],
    )?;
    state.pack(&mut distributor.try_borrow_mut_data()?);
    msg!("airdrop: claimed {}", amount);
    Ok(())
}

fn burn_unclaimed(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let accounts = &mut accounts.iter();
    let distributor = next_account_info(accounts)?;
    let vault = next_account_info(accounts)?;
    let mint = next_account_info(accounts)?;
    let token_program = next_account_info(accounts)?;
    check_programs(token_program, None)?;
    let mut state = load(program_id, distributor)?;
    if mint.key != &state.mint {
        return Err(AirdropError::InvalidMint.into());
    }
    if Clock::get()?.unix_timestamp < state.deadline {
        return Err(AirdropError::ClaimsOpen.into());
    }
    if vault.key != &vault_address(distributor.key, mint.key) || vault.owner != token_program.key {
        return Err(AirdropError::InvalidVault.into());
    }
    let amount = StateWithExtensions::<Account>::unpack(&vault.try_borrow_data()?)?.base.amount;
    if amount == 0 {
        return Ok(());
    }
    let decimals = mint_decimals(mint)?;
    let bump_seed = [state.bump];
    invoke_signed(
        &spl_token_2022::instruction::burn_checked(token_program.key, vault.key, mint.key, distributor.key, &[], amount, decimals)?,
        &[vault.clone(), mint.clone(), distributor.clone(), token_program.clone()],
        &[&[DISTRIBUTOR_SEED, state.mint.as_ref(), state.creator.as_ref(), &bump_seed]],
    )?;
    state.burned = state.burned.checked_add(amount).ok_or(AirdropError::Overflow)?;
    state.pack(&mut distributor.try_borrow_mut_data()?);
    msg!("airdrop: burned {} unclaimed", amount);
    Ok(())
}

fn check_programs(token_program: &AccountInfo, system: Option<&AccountInfo>) -> ProgramResult {
    if token_program.key != &spl_token_2022::id() || !token_program.executable {
        return Err(AirdropError::InvalidTokenProgram.into());
    }
    if system.is_some_and(|system| system.key != &system_program::id()) {
        return Err(AirdropError::InvalidSystemProgram.into());
    }
    Ok(())
}

fn load(program_id: &Pubkey, distributor: &AccountInfo) -> Result<Distributor, ProgramError> {
    if distributor.owner != program_id {
        return Err(AirdropError::InvalidDistributor.into());
    }
    let state = Distributor::unpack(&distributor.try_borrow_data()?)?;
    let seed = [state.bump];
    let expected = Pubkey::create_program_address(&[DISTRIBUTOR_SEED, state.mint.as_ref(), state.creator.as_ref(), &seed], program_id)
        .map_err(|_| AirdropError::InvalidDistributor)?;
    if distributor.key != &expected {
        return Err(AirdropError::InvalidDistributor.into());
    }
    Ok(state)
}

fn mint_decimals(mint: &AccountInfo) -> Result<u8, ProgramError> {
    Ok(StateWithExtensions::<Mint>::unpack(&mint.try_borrow_data()?)?.base.decimals)
}

/// Creates a program-owned account at a PDA. An address someone prefunded with lamports is topped up, allocated and
/// assigned instead, so nobody can block a claim by sending SOL to its marker address.
fn create_owned<'a>(payer: &AccountInfo<'a>, account: &AccountInfo<'a>, system: &AccountInfo<'a>, owner: &Pubkey,
    size: usize, seeds: &[&[u8]]) -> ProgramResult {
    let required = Rent::get()?.minimum_balance(size);
    if account.lamports() == 0 {
        return invoke_signed(&system_instruction::create_account(payer.key, account.key, required, size as u64, owner),
            &[payer.clone(), account.clone(), system.clone()], &[seeds]);
    }
    if account.owner != &system_program::id() || !account.data_is_empty() {
        return Err(AirdropError::AlreadyInitialized.into());
    }
    let missing = required.saturating_sub(account.lamports());
    if missing > 0 {
        invoke(&system_instruction::transfer(payer.key, account.key, missing), &[payer.clone(), account.clone(), system.clone()])?;
    }
    invoke_signed(&system_instruction::allocate(account.key, size as u64), &[account.clone(), system.clone()], &[seeds])?;
    invoke_signed(&system_instruction::assign(account.key, owner), &[account.clone(), system.clone()], &[seeds])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    #[test]
    fn distributor_round_trips() {
        let state = Distributor { mint: key(1), creator: key(2), root: [3; 32], total: 4, deadline: -5, claimed: 6, claims: 7, burned: 8,
            fee_basis_points: 100, fee_maximum: u64::MAX, bump: 9 };
        let mut data = [0u8; DISTRIBUTOR_SIZE];
        state.pack(&mut data);
        assert_eq!(Distributor::unpack(&data).unwrap(), state);
        assert!(Distributor::unpack(&data[1..]).is_err());
        data[0] = 0;
        assert!(Distributor::unpack(&data).is_err());
    }

    #[test]
    fn proofs_verify_only_their_own_leaf() {
        let distributor = key(10);
        let leaves: Vec<[u8; 32]> = (0..3).map(|i| leaf(&distributor, &key(20 + i), 100 + i as u64)).collect();
        let ab = node(&leaves[0], &leaves[1]);
        let root = node(&ab, &leaves[2]);
        assert!(verify(&[leaves[1], leaves[2]], &root, leaves[0]));
        assert!(verify(&[leaves[0], leaves[2]], &root, leaves[1]));
        assert!(verify(&[ab], &root, leaves[2]));
        // Wrong amount, wrong wallet or another distributor.
        assert!(!verify(&[leaves[1], leaves[2]], &root, leaf(&distributor, &key(20), 101)));
        assert!(!verify(&[leaves[1], leaves[2]], &root, leaf(&distributor, &key(21), 100)));
        assert!(!verify(&[leaves[1], leaves[2]], &root, leaf(&key(11), &key(20), 100)));
    }

    #[test]
    fn fee_matches_token_2022() {
        use spl_token_2022::extension::transfer_fee::TransferFee;
        for (amount, basis_points, maximum) in [(0, 100, u64::MAX), (1, 100, u64::MAX), (99, 100, u64::MAX), (101, 100, u64::MAX),
            (1_000_000_000_000_000, 100, u64::MAX), (u64::MAX, 10_000, u64::MAX), (u64::MAX, 100, 5), (12_345, 0, 7), (12_345, 250, 10)] {
            let reference = TransferFee { epoch: 0.into(), maximum_fee: maximum.into(), transfer_fee_basis_points: basis_points.into() };
            assert_eq!(fee_for(amount, basis_points, maximum), reference.calculate_fee(amount), "{amount} {basis_points} {maximum}");
        }
    }

    #[test]
    fn node_is_order_independent_and_domain_separated() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert_eq!(node(&a, &b), node(&b, &a));
        assert_ne!(node(&a, &b), hashv(&[LEAF_DOMAIN, &a, &b]).to_bytes());
    }
}
