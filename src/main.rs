use std::{thread, collections::{HashMap, HashSet}, time::Duration, vec, ops::*};
use ark_ec::{CurveGroup, AffineRepr, pairing::Pairing, Group};
use ark_ff::Field;
use ark_poly::{ GeneralEvaluationDomain, EvaluationDomain, Polynomial, univariate::{DensePolynomial, DenseOrSparsePolynomial}, DenseUVPolynomial};
use ark_serialize::CanonicalSerialize;
use ark_std::{Zero, One, UniformRand};
use async_std::task;
//use std::sync::mpsc;
use futures::channel::*;
use clap::Parser;
use num_bigint::BigUint;
use serde_json::json;

mod network;
mod evaluator;
mod address_book;
mod common;
mod utils;
mod kzg;

use address_book::*;
use evaluator::*;
use common::*;

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long)]
    id: String,

    /// Fixed value to generate deterministic peer id
    #[clap(long)]
    seed: u8,
}

fn parse_addr_book_from_json() -> Pok3rAddrBook {
    let config = json!({
        "addr_book": [ //addr_book is a list of ed25519 pubkeys
            "12D3KooWPjceQrSwdWXPyLLeABRXmuqt69Rg3sBYbU1Nft9HyQ6X", //pubkey of node with seed 1
            "12D3KooWH3uVF6wv47WnArKHk5p6cvgCJEb74UTmxztmQDc298L3", //pubkey of node with seed 2
            "12D3KooWQYhTNQdmr3ArTeUHRYzFg94BKyTkoWBDWez9kSCVe2Xo"  //pubkey of node with seed 3
        ]
    });
    let mut peers: Vec<String> = config["addr_book"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| String::from(o.as_str().unwrap()))
        .collect();
    peers.sort();

    let mut output: Pok3rAddrBook = HashMap::new();
    let mut counter = 0;
    for peer in peers {
        let pok3rpeer = Pok3rPeer {
            peer_id: peer.to_owned(),
            node_id: counter,
        };

        output.insert(peer, pok3rpeer);
        counter += 1;
    }
    output
}

#[async_std::main]
async fn main() {
    let args = Args::parse();

    //these channels will connect the evaluator and the network daemons
    let (mut n2e_tx, n2e_rx) = mpsc::unbounded::<EvalNetMsg>();
    let (e2n_tx, e2n_rx) = mpsc::unbounded::<EvalNetMsg>();

    let netd_handle = thread::spawn(move || {
        let result = task::block_on(
            network::run_networking_daemon(
                args.seed, 
                &parse_addr_book_from_json(), 
                &mut n2e_tx,
                e2n_rx)
        );
        if let Err(err) = result {
            eprint!("Networking error {:?}", err);
        }
    });
    
    let addr_book = parse_addr_book_from_json();
    let mut mpc = Evaluator::new(&args.id, addr_book, e2n_tx, n2e_rx).await;

    //this is a hack until we figure out
    task::block_on(async {
        task::sleep(Duration::from_secs(1)).await;
        println!("After sleeping for 1 second.");
    });

    mpc.test_networking().await;
    evaluator::perform_sanity_testing(&mut mpc).await;
    test_sigma(&mut mpc).await;
    test_local_kzg();
    test_dist_kzg(&mut mpc).await;
    test_share_poly_mult(&mut mpc).await;

    // Actual protocol
    let (card_share_handles, card_shares) = shuffle_deck(&mut mpc).await;
    
    let perm_proof = compute_permutation_argument(
        &mut mpc, 
        card_share_handles.clone(), 
        &card_shares
    ).await;

    let verified = verify_permutation_argument(&perm_proof).await;

    if verified {
        println!("Permutation argument verified");
    } else {
        println!("Permutation argument verification failed");
    }

    // Get a random public key pk in G2 - for testing (should be generated by DKG)
    let pk = G2::rand(&mut rand::thread_rng());

    // Get random ids as byte strings
    let mut ids = vec![];
    for i in 0..64 {
        let id = BigUint::from(i as u8);
        ids.push(id);
    }

    let encrypt_proof = encrypt_and_prove(&mut mpc, card_share_handles.clone(), perm_proof.f_com, pk, ids).await;
    let verified = local_verify_encryption_proof(&encrypt_proof).await;

    if verified {
        println!("Encryption proof verified");
    } else {
        println!("Encryption proof verification failed");
    }

    //eval_handle.join().unwrap();
    netd_handle.join().unwrap();
}

fn map_roots_of_unity_to_cards() -> HashMap<F, String> {
    let mut output: HashMap<F, String> = HashMap::new();
    
    // get generator for the 64 powers of 64-th root of unity
    let ω = utils::multiplicative_subgroup_of_size(64);

    // map each power to a card
    // map 64 cards
    for i in 0..64 {
        let ω_pow_i = utils::compute_power(&ω, i as u64);
        let card_name = i.to_string();
        output.insert(ω_pow_i, card_name);
    }

    // //and 12 jokers
    // for i in 52..64 {
    //     let ω_pow_i = utils::compute_power(&ω, i as u64);
    //     output.insert(ω_pow_i, String::from("Joker"));
    // }

    output
}

async fn shuffle_deck(evaluator: &mut Evaluator) -> (Vec<String>, Vec<F>) {
    println!("-------------- Starting Pok3r shuffle -----------------");

    //step 1: parties invoke F_RAN to obtain [sk]
    let sk = evaluator.ran();

    //stores (handle, wire value) pairs
    let mut card_share_handles = Vec::new();
    let mut card_share_values = Vec::new();
    //stores set of card prfs encountered
    let mut prfs = HashSet::new();

    // Compute prfs for cards 52 to 63 and add to prfs first
    // So that the positions of these cards are fixed in the permutation
    for i in 52..64 {
        let h_r = evaluator.ran();
        let (h_a, h_b, h_c) = evaluator.beaver().await;

        let ω = utils::multiplicative_subgroup_of_size(64);
        let ω_pow_i = utils::compute_power(&ω, i as u64);

        // y_i = g^{1 / (sk + w_i)}
        let denom = evaluator.clear_add(&sk, ω_pow_i);
        let t_i = evaluator.inv(
            &denom,
            &h_r,
            (&h_a, &h_b, &h_c)
        ).await;
        let y_i = evaluator.output_wire_in_exponent(&t_i).await;

        prfs.insert(y_i.clone());
        let handle = evaluator.fixed_wire_handle(ω_pow_i).await;
        card_share_handles.push(handle.clone());
        card_share_values.push(evaluator.get_wire(&handle));
    }

    // TODO : After batching, this cannot be variable - must run ~1275 times or so to get enough cards with high probability
    while card_share_values.len() < 64 { // until you get the other 52 cards
        let h_r = evaluator.ran();
        let (h_a, h_b, h_c) = evaluator.beaver().await;

        let a_i = evaluator.ran();
        let c_i = evaluator.ran_64(&a_i).await;
        let t_i = evaluator.add(&c_i, &sk);
        let t_i = evaluator.inv(
            &t_i,
            &h_r,
            (&h_a, &h_b, &h_c)
        ).await;

        // y_i = g^{1 / (sk + w_i)}
        let y_i = evaluator.output_wire_in_exponent(&t_i).await;

        //add card if it hasnt been seen before
        if ! prfs.contains(&y_i) {
            prfs.insert(y_i.clone());
            card_share_handles.push(c_i.clone());
            card_share_values.push(evaluator.get_wire(&c_i));
        }
    }

    // TODO - after batching, check that the length of card_share_values and handles are 64 and panic if not


    // For printing purposes 
    let card_mapping = map_roots_of_unity_to_cards();
    for h_c in &card_share_handles {
        let opened_card = evaluator.output_wire(&h_c).await;
        println!("{}", card_mapping.get(&opened_card).unwrap());
    }

    println!("-------------- Ending Pok3r shuffle -----------------");
    return (card_share_handles.clone(), card_share_values);
}

async fn compute_permutation_argument(
    evaluator: &mut Evaluator,
    card_share_handles: Vec<String>,
    card_share_values: &Vec<F>
) -> PermutationProof {
    // Compute r_i and r_i^-1
    // 1: for i ← 0 . . . 65 (in parallel) do
    // 2: Parties invoke FRANp to obtain [ri]p
    // 3: Parties invoke FINV with input [ri]p to obtain [ri]−1
    // 4: end for
    let mut r_is = vec![]; //vector of (handle, share_value) pairs
    let mut r_inv_is = vec![]; //vector of (handle, share_value) pairs

    for _i in 0..65 {
        // Beaver triple for inverse
        let (h_a, h_b, h_c) = evaluator.beaver().await;
        // Random value for inverse
        let h_t = evaluator.ran();

        let h_r_i = evaluator.ran();
        let h_r_inv_i = evaluator.inv(
            &h_r_i,
            &h_t,
            (&h_a, &h_b, &h_c)
        ).await;

        r_is.push((h_r_i.clone(), evaluator.get_wire(&h_r_i)));
        r_inv_is.push((h_r_inv_i.clone(), evaluator.get_wire(&h_r_inv_i)));
    }

    // Compute b_i from r_i and r_i^-1
    // 5: for i ← 0 . . . 64 (in parallel) do
    // 6: Parties invoke FMULT with inputs ([r-1 0]p, [ri+1]p) to obtain [bi]p.
    // 7: end for
    let mut b_is = vec![]; //vector of (handle, share_value) pairs
    for i in 0..64 {
        // Beaver triple for mult
        let (h_a, h_b, h_c) = evaluator.beaver().await;

        let h_r_inv_0 = &r_inv_is.get(0).unwrap().0;
        let h_r_i_plus_1 = &r_is.get(i+1).unwrap().0;

        let h_b_i = evaluator.mult(
            h_r_inv_0,
            h_r_i_plus_1,
            (&h_a, &h_b, &h_c)
        ).await;

        b_is.push((h_b_i.clone(), evaluator.get_wire(&h_b_i)));
    }

    // 8: Interpret the vector fi as evaluations of a polynomial f(X).
    let f_name = String::from("perm_f");
    let f_share = 
        utils::interpolate_poly_over_mult_subgroup(card_share_values);
    let f_share_com = utils::commit_poly(&f_share);

    // Commit to f(X)
    let f_com = evaluator.add_g1_elements_from_all_parties(&f_share_com, &f_name).await;

    // 9: Define the degree-64 polynomial v(X) such that the evaluation vector is (1, ω, . . . , ω63)
    // This polynomial is the unpermuted vector of cards 
    let ω = utils::multiplicative_subgroup_of_size(64);
    let v_evals: Vec<F> = (0..64)
        .into_iter()
        .map(|i| utils::compute_power(&ω, i as u64))
        .collect();
    let v = utils::interpolate_poly_over_mult_subgroup(&v_evals);
    
    // Commit to v(X)
    let v_com = utils::commit_poly(&v);

    // 12: Parties locally compute γ1 = FSHash(C,V )
    // Hash v_com and f_com to obtain randomness for batching
    let mut v_bytes = Vec::new();
    let mut f_bytes = Vec::new();

    v_com.serialize_uncompressed(&mut v_bytes).unwrap();
    f_com.serialize_uncompressed(&mut f_bytes).unwrap();

    let y1 = utils::fs_hash(vec![&v_bytes, &f_bytes], 1)[0];

    // 13: Locally compute g(X) shares from f(X) shares
    let mut g_eval_shares = vec![];
    let mut h_g_shares = vec![];
    for i in 0..64 {
        // let g_share_i = card_share_values[i] + y1;
        // g_eval_shares.push(g_i);

        // Get a handle for g_i for later
        h_g_shares.push(evaluator.clear_add(&card_share_handles[i], y1));

        let g_share_i = evaluator.get_wire(&h_g_shares[i].clone());
        g_eval_shares.push(g_share_i);
    }

    let g_share_poly = 
        utils::interpolate_poly_over_mult_subgroup(&g_eval_shares.clone());

    // Commit to g(X)
    let g_share_com = utils::commit_poly(&g_share_poly);
    let g_com = evaluator.add_g1_elements_from_all_parties(&g_share_com, &String::from("perm_g")).await;

    // Assert that g(X) is correctly computed in both prover and verifier
    // Commit to constant polynomial const(x) = y1
    let const_y1 = DensePolynomial::from_coefficients_vec(vec![y1]);
    let const_com_y1 = utils::commit_poly(&const_y1);

    let g_com_verifier = (f_com.clone() + const_com_y1).into_affine();
    assert_eq!(g_com, g_com_verifier);

    // 14: Compute h(X) = v(X) + y1
    let mut h_evals = vec![];
    for i in 0..64 {
        let h_i = v_evals[i] + y1;
        h_evals.push(h_i);
    }
    let h_poly = utils::interpolate_poly_over_mult_subgroup(&h_evals);

    // Compute s_i' and t_i'
    let mut t_prime_is = vec![];

    // 15: for i ← 0 . . . 63 (in parallel) do
    // 16: Parties invoke FMULT with inputs (h−1i ·[gi]p, [ri]p) to get [s′i]p.
    // 17: Parties invoke FMULT with inputs ([s′i]p, [r−1i+1]p) to get [t'i]p.
    // 18: Parties reconstruct t′i.
    // 19: end for
    for i in 0..64 {
        // Beaver triple for two mults
        let (h_a1, h_b1, h_c1) = evaluator.beaver().await;
        let (h_a2, h_b2, h_c2) = evaluator.beaver().await;

        let h_r_i = &r_is.get(i).unwrap().0;
        let h_r_inv_i_plus_1 = &r_inv_is.get(i+1).unwrap().0;
        
        // Get a handle for g_i and scale with h_i^inv
        let h_g_i = &h_g_shares[i];
        let h_inv_i = h_evals[i].inverse().unwrap();
        let h_h_inv_g_i = &evaluator.scale(h_g_i, h_inv_i);

        // Parties invoke FMULT with inputs (h−1
        // i ·[gi]p, [ri]p)
        // to get [s′
        // i]p
        let s_prime_i = evaluator.mult(
            h_r_i,
            h_h_inv_g_i,
            (&h_a1, &h_b1, &h_c1)
        ).await;

        // Parties invoke FMULT with inputs ([s′
        // i]p, [r−1
        // i+1]p) to
        // get [t′
        // i ]p
        let t_prime_i = evaluator.mult(
            h_r_inv_i_plus_1,
            &s_prime_i,
            (&h_a2, &h_b2, &h_c2)
        ).await;

        let t_prime_i = evaluator.output_wire(&t_prime_i).await;
        t_prime_is.push(t_prime_i);
    }

    // Locally compute t_i
    // 20: for i ← 0 . . . 63 do
    // 21: Parties locally compute [ti]p ← [bi]p · ∏ij=0 t′j
    // 22: end for
    let mut t_is = vec![];
    for i in 0..64 {
        // let tmp = product of t'_i from 0 to i
        let mut tmp = F::one();
        for j in 0..(i+1) {
            tmp = tmp * t_prime_is[j];
        }

        // Multiply by b_i to remove random masks
        let t_i = evaluator.scale(&b_is[i].0, tmp);       

        t_is.push((t_i.clone(), evaluator.get_wire(&t_i)));
    }

    // Commit to t(X)
    let t_shares : &Vec<F> = &t_is.clone()
        .into_iter()
        .map(|x| x.1)
        .collect();
    let t_share_poly = utils::interpolate_poly_over_mult_subgroup(&t_shares);
    let t_share_com = utils::commit_poly(&t_share_poly);
    let t_com = evaluator.add_g1_elements_from_all_parties(&t_share_com, &String::from("t")).await;

    let tx_by_omega_share_poly = utils::poly_domain_div_ω(&t_share_poly, &ω);

    // Need to show that t(X) / t(X/ω) = g(X) / h(X)
    // 24: Compute [d(X)] as [d(X)] = h(X) * [t(X)] − [g(X) * t(X/ω)]
    let h_t_share_poly = h_poly.mul(&t_share_poly);
    let g_tx_by_omega_share_poly = evaluator.share_poly_mult(
        g_share_poly.clone(), 
        tx_by_omega_share_poly.clone()
    ).await;
    
    let d_share_poly = h_t_share_poly.sub(&g_tx_by_omega_share_poly);

    // Sanity check - d(X) should be zero at powers of omega
    for i in 0..64 {
        let ω_pow_i = utils::compute_power(&ω, i as u64);
        let d_i = evaluator.share_poly_eval(d_share_poly.clone(), ω_pow_i).await;
        let tmp = evaluator.output_wire(&d_i).await;
        assert_eq!(tmp, F::zero(), "d(X) is not zero at ω^{i}");
    }

    // Compute q(X) and r(X) as quotient and remainder of d(X) / (X^64 - 1)
    // TOASSERT - Reconstructed r(X) should be 0
    let domain = GeneralEvaluationDomain::<F>::new(64).unwrap();
    let (q_share_poly, _) = d_share_poly.divide_by_vanishing_poly(domain).unwrap();

    // Commit to q(X)
    let q_share_com = utils::commit_poly(&q_share_poly);
    let q_com = evaluator.add_g1_elements_from_all_parties(&q_share_com, &String::from("perm_q")).await;

    // Compute y2 = hash(v_com, f_com, q_com, t_com, g_com)
    let mut v_bytes = Vec::new();
    let mut f_bytes = Vec::new();
    let mut q_bytes = Vec::new();
    let mut t_bytes = Vec::new();
    let mut g_bytes = Vec::new();

    v_com.serialize_uncompressed(&mut v_bytes).unwrap();
    f_com.serialize_uncompressed(&mut f_bytes).unwrap();
    q_com.serialize_uncompressed(&mut q_bytes).unwrap();
    t_com.serialize_uncompressed(&mut t_bytes).unwrap();
    g_com.serialize_uncompressed(&mut g_bytes).unwrap();

    let y2 = utils::fs_hash(vec![&v_bytes, &f_bytes, &q_bytes, &t_bytes, &g_bytes], 1)[0];

    // Compute polyevals and proofs
    let w = utils::multiplicative_subgroup_of_size(64);
    let w63 = utils::compute_power(&w, 63);

    // Evaluate t(x) at w^63
    let h_y1 = evaluator.share_poly_eval(t_share_poly.clone(), w63).await;
    let pi_1 = evaluator.eval_proof_with_share_poly(t_share_poly.clone(), w63, String::from("perm_pi_1")).await;

    // Evaluate t(x) at y2
    let h_y2 = evaluator.share_poly_eval(t_share_poly.clone(), y2).await;
    let pi_2 = evaluator.eval_proof_with_share_poly(t_share_poly.clone(), y2, String::from("perm_pi_2")).await;

    // Evaluate t(x) at y2 / w
    let h_y3 = evaluator.share_poly_eval(t_share_poly.clone(), y2 / w).await;
    let pi_3 = evaluator.eval_proof_with_share_poly(t_share_poly.clone(), y2 / w, String::from("perm_pi_3")).await;

    // Evaluate g(x) at y2
    let h_y4 = evaluator.share_poly_eval(g_share_poly.clone(), y2).await;
    let pi_4 = evaluator.eval_proof_with_share_poly(g_share_poly.clone(), y2, String::from("perm_pi_4")).await;

    // Evaluate q(x) at y2
    let h_y5 = evaluator.share_poly_eval(q_share_poly.clone(), y2).await;
    let pi_5 = evaluator.eval_proof_with_share_poly(q_share_poly.clone(), y2, String::from("perm_pi_5")).await;

    PermutationProof {
        y1: evaluator.output_wire(&h_y1).await,
        y2: evaluator.output_wire(&h_y2).await,
        y3: evaluator.output_wire(&h_y3).await,
        y4: evaluator.output_wire(&h_y4).await,
        y5: evaluator.output_wire(&h_y5).await,
        pi_1,
        pi_2,
        pi_3,
        pi_4,
        pi_5,
        f_com,
        q_com,
        t_com
    }
}

async fn verify_permutation_argument(
    perm_proof: &PermutationProof,
) -> bool {
    let mut b = true;

    // Compute v(X) from powers of w
    let w = utils::multiplicative_subgroup_of_size(64);
    let w63 = utils::compute_power(&w, 63);

    let v_evals: Vec<F> = (0..64)
        .into_iter()
        .map(|i| utils::compute_power(&w, i as u64))
        .collect();

    let v = utils::interpolate_poly_over_mult_subgroup(&v_evals);
    let v_com = utils::commit_poly(&v);

    // Compute hash1 and hash2
    let mut v_bytes = Vec::new();
    let mut f_bytes = Vec::new();
    let mut q_bytes = Vec::new();
    let mut t_bytes = Vec::new();
    let mut g_bytes = Vec::new();

    v_com.serialize_uncompressed(&mut v_bytes).unwrap();
    perm_proof.f_com.serialize_uncompressed(&mut f_bytes).unwrap();

    let hash1 = utils::fs_hash(vec![&v_bytes, &f_bytes], 1)[0];

    // Compute g_com from f_com
    let const_y1 = DensePolynomial::from_coefficients_vec(vec![hash1]);
    let const_com_y1 = utils::commit_poly(&const_y1);

    let g_com = (perm_proof.f_com.clone() + const_com_y1).into_affine();

    perm_proof.q_com.serialize_uncompressed(&mut q_bytes).unwrap();
    perm_proof.t_com.serialize_uncompressed(&mut t_bytes).unwrap();
    g_com.serialize_uncompressed(&mut g_bytes).unwrap();

    let hash2 = utils::fs_hash(vec![&v_bytes, &f_bytes, &q_bytes, &t_bytes, &g_bytes], 1)[0];
    
    // Check all evaluation proofs
    b = b & utils::kzg_check(
        &perm_proof.t_com,
        &w63,
        &perm_proof.y1,
        &perm_proof.pi_1
    );

    b = b & utils::kzg_check(
        &perm_proof.t_com,
        &hash2,
        &perm_proof.y2,
        &perm_proof.pi_2
    );

    b = b & utils::kzg_check(
        &perm_proof.t_com,
        &(hash2 / w),
        &perm_proof.y3,
        &perm_proof.pi_3
    );

    b = b & utils::kzg_check(
        &g_com,
        &(hash2),
        &perm_proof.y4,
        &perm_proof.pi_4
    );

    b = b & utils::kzg_check(
        &perm_proof.q_com,
        &hash2,
        &perm_proof.y5,
        &perm_proof.pi_5
    );

    // y1 = t(w^63)
    // y2 = t(hash2)
    // y3 = t(hash2 / w)
    // y4 = g(hash2)
    // y5 = q(hash2)
    // Check 1 : y2 * (v(hash2) + hash1) - y3 * y4 = y5 * (hash2^k - 1)
    let tmp1 = perm_proof.y2 * (v.evaluate(&hash2) + hash1);
    let tmp2 = perm_proof.y3 * perm_proof.y4;
    let tmp3 = perm_proof.y5 * (hash2.pow([64]) - F::one());

    b = b & (tmp1 - tmp2 == tmp3);

    if tmp1 - tmp2 != tmp3 {
        println!("VerifyPerm - Check 1 failed");
    }

    // Check 2 : y1 = 1
    b = b & (perm_proof.y1 == F::one());

    if perm_proof.y1 != F::one() {
        println!("VerifyPerm - Check 2 failed");
    }
    
    b
}

// Proves the composite statement
pub async fn dist_sigma_proof(
    evaluator: &mut Evaluator,
    base_1: &G1,
    base_2: &G1,
    base_3: &Gt,
    wit_1_handles: Vec<String>,
    wit_2_handle: String,
    lin_comb_ran: Vec<F>
) -> SigmaProof {
    // Message 1
    // a1 = base_1^b1
    // a2 = base_2^b2
    // a3 = base_2^b1 * base_3^b2
    let z2 = evaluator.ran();
    let a1 = evaluator.exp_and_reveal_g1(
        vec![base_1.clone()], 
        vec![z2.clone()], 
        &String::from("a1")
    ).await;
    let a2 = evaluator.exp_and_reveal_g1(
        vec![base_2.clone()], 
        vec![z2.clone()], 
        &String::from("a2")
    ).await;
    let a3 = evaluator.exp_and_reveal_gt(
        vec![base_3.clone()], 
        vec![z2.clone()], 
        &String::from("a3")
    ).await;
    let a4 = evaluator.exp_and_reveal_gt(
        vec![Gt::generator()], 
        vec![z2.clone()], 
        &String::from("a4")
    ).await;

    // FS Hash of a1,a2,a3 
    let (mut a1_bytes, mut a2_bytes, mut a3_bytes, mut a4_bytes): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) 
        = (Vec::new(),Vec::new(),Vec::new(),Vec::new());

    a1.serialize_uncompressed(&mut a1_bytes).unwrap();
    a2.serialize_uncompressed(&mut a2_bytes).unwrap();
    a3.serialize_uncompressed(&mut a3_bytes).unwrap();
    a4.serialize_uncompressed(&mut a4_bytes).unwrap();
    
    let gamma = utils::fs_hash(vec![&a1_bytes, &a2_bytes, &a3_bytes, &a4_bytes], 1);

    // Message 3
    let mut h_y = evaluator.scale(&wit_2_handle.clone(), gamma[0]);
    h_y = evaluator.add(&h_y,&z2);
    let y = evaluator.output_wire(&h_y).await;

    // x = gamma * sum_i (lin_comb_ran[i] * wit_1_handles[i]) + z2
    let mut h_x = evaluator.scale(&wit_1_handles[0], lin_comb_ran[0]);

    for i in 1..64 {
        let tmp = evaluator.scale(&wit_1_handles[i], lin_comb_ran[i]);
        h_x = evaluator.add(&tmp, &h_x);
    }
    h_x = evaluator.scale(&h_x, gamma[0]);
    h_x = evaluator.add(&h_x, &z2);

    let x = evaluator.output_wire(&h_x).await;
    
    SigmaProof{a1,a2,a3,a4,x,y}
}

// Batch the bases before calling this
// Verifies custom sigma proof generated by dist_sigma_proof
pub fn local_verify_sigma_proof(
    c: &G1, d_batch: &G1, 
    g: &G1, c_1: &G1,
    e_batch: &Gt, c2_batch: &Gt,
    sigma: &SigmaProof
) -> bool {
    // Hash a1,a2,a3,a4 to get gamma
    let (mut a1_bytes, mut a2_bytes, mut a3_bytes, mut a4_bytes): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) 
        = (Vec::new(),Vec::new(),Vec::new(),Vec::new());

    sigma.a1.serialize_uncompressed(&mut a1_bytes).unwrap();
    sigma.a2.serialize_uncompressed(&mut a2_bytes).unwrap();
    sigma.a3.serialize_uncompressed(&mut a3_bytes).unwrap();
    sigma.a4.serialize_uncompressed(&mut a4_bytes).unwrap();

    let gamma = utils::fs_hash(vec![&a1_bytes, &a2_bytes, &a3_bytes, &a4_bytes], 1);

    let mut b = true;

    // Verify statement 1 : C^x = D_batch^gamma * a1
    let lhs = c.mul(sigma.x);
    let rhs = (d_batch.mul(gamma[0])).add(sigma.a1);
    if ! lhs.eq(&rhs) {
        println!("SigmaProof - Check 1 fail");
        b = false;
    }

    // Verify statement 2 : g^y = c_1^gamma * a2
    let lhs = g.mul(sigma.y);
    let rhs = c_1.mul(gamma[0]).add(sigma.a2);
    if ! lhs.eq(&rhs) {
        println!("SigmaProof - Check 2 fail");
        b = false;
    }

    // Verify statement 3 : g^x * e_batch^y = c2_batch^gamma * a3 * a4
    let lhs = e_batch.mul(sigma.y).add(Gt::generator().mul(sigma.x));
    let rhs = c2_batch.mul(gamma[0]).add(sigma.a4).add(sigma.a3);
    if ! lhs.eq(&rhs) {
        println!("SigmaProof - Check 3 fail");
        b = false;
    }  

    b
}

async fn encrypt_and_prove(
    evaluator: &mut Evaluator,
    card_handles: Vec<String>,
    card_commitment: G1,
    pk: G2,
    ids: Vec<BigUint>
) -> EncryptProof {
    // Get all cards from card handles
    let mut cards = vec![];
    for h in card_handles.clone() {
        cards.push(evaluator.get_wire(&h));
    }

    // Sample common randomness for encryption
    let r = evaluator.ran();

    let mut z_is = vec![]; //vector of (handle, share_value) pairs
    let mut d_is = vec![]; //vector of scaled commitments 
    let mut v_is = vec![]; //vector of (handle, share_value) pairs
    let mut v_is_reconstructed = vec![]; //vector of reconstructed v_i values
    let mut pi_is = vec![]; //vector of evaluation proofs

    let mut c1_is = vec![]; //vector of ciphertexts
    let mut c2_is = vec![]; //vector of ciphertexts

    // Compute shares of plain quotient polynomial commitment
    let mut pi_plain_vec = vec![]; //vector of plain non-reconstructed evaluation proofs
    let w = utils::multiplicative_subgroup_of_size(64);

    for i in 0..64 {
        let z = utils::compute_power(&w, i);
        let pi_plain_i = evaluator.eval_proof(card_handles.clone(), z, format!("pi_plain_{}", i)).await;
        pi_plain_vec.push(pi_plain_i);
    }
    

    for i in 0..64 {
        let (h_a, h_b, h_c) = evaluator.beaver().await;

        // Sample mask to be encrypted
        let z_i = evaluator.ran();
        z_is.push((z_i.clone(), evaluator.get_wire(&z_i)));

        // Encrypt the mask to id_i
        let (c1_i, c2_i) = 
            evaluator.dist_ibe_encrypt(&card_handles[i], &r, &pk, ids[i].clone()).await;
        c1_is.push(c1_i);
        c2_is.push(c2_i); 

        // Compute d_i = C_i^z_i
        let d_i = 
            evaluator.exp_and_reveal_g1(vec![card_commitment], vec![z_i.clone()], &format!("{}/{}", "D_", i)).await;
        d_is.push(d_i.clone());

        // Compute v_i = z_i * card_i
        let v_i = evaluator.mult(&z_i, &card_handles[i], (&h_a, &h_b, &h_c)).await;        
        v_is.push((v_i.clone(), evaluator.get_wire(&v_i)));
        v_is_reconstructed.push(evaluator.output_wire(&v_i).await);

        // TODO: batch this
        // Evaluation proofs of d_i at \omega^i to v_i 
        // Currently computed by raising the plain eval proof shares to the power z_i and then reconstructing the group elements

        let pi_i_share = pi_plain_vec[i].clone().mul(z_is[i].1).into_affine();
        let pi_i = 
            evaluator.add_g1_elements_from_all_parties(&pi_i_share, &format!("{}/{}", "pi_", i)).await;
        pi_is.push(pi_i);

    }

    // Hash to obtain randomness for batching

    let tmp_proof = EncryptProof{
        pk: pk.clone(),
        ids: ids.clone(),
        card_commitment: card_commitment.clone(),
        masked_commitments: d_is.clone(),
        masked_evals: v_is_reconstructed.clone(),
        eval_proofs: pi_is.clone(),
        ciphertexts: c1_is.clone().into_iter().zip(c2_is.clone().into_iter()).collect(),
        sigma_proof: None,
    };

    let s = utils::fs_hash(vec![&tmp_proof.to_bytes()], 64);

    // Compute batched pairing base for sigma proof
    let mut e_batch = Gt::zero();

    for i in 0..64 {
        // TODO: fix this. Need proper hash to curve
        let x_f = F::from(ids[i].clone());
        let hash_id = G1::generator().mul(x_f);

        let h = <Curve as Pairing>::pairing(hash_id, pk);

        e_batch = e_batch.add(h.mul(s[i]));
    }

    let mut wit_1 = vec![];
    
    for i in 0..64 {
        wit_1.push(z_is[i].clone().0);
    }

    let proof = dist_sigma_proof(
            evaluator,
            &card_commitment,
            &G1::generator(),
            &e_batch,
            wit_1,
            r,
            s).await;

    EncryptProof {
        pk: pk.clone(),
        ids: ids,
        card_commitment: card_commitment,
        masked_commitments: d_is,
        masked_evals: v_is_reconstructed,
        eval_proofs: pi_is,
        ciphertexts: c1_is.into_iter().zip(c2_is.into_iter()).collect(),
        sigma_proof: Some(proof),
    }
}

async fn local_verify_encryption_proof(
    proof: &EncryptProof,
) -> bool {
    // Check that all ciphertexts share the same randomness
    let c1 = proof.ciphertexts[0].0.clone();
    for i in 1..64 {
        if proof.ciphertexts[i].0 != c1 {
            return false;
        }
    }

    // Check the sigma proof

    // Hash to obtain randomness for batching
    let s = utils::fs_hash(vec![&proof.to_bytes()], 64);

    // Compute e_batch
    let mut e_batch = Gt::zero();

    for i in 0..64 {
        let x_f = F::from(proof.ids[i].clone());
        let hash_id = G1::generator().mul(x_f);

        let h = <Curve as Pairing>::pairing(hash_id, &proof.pk);

        e_batch = e_batch.add(h.mul(s[i]));
    }

    // Compute d_batch
    let mut d_batch = G1::zero();

    for i in 0..64 {
        d_batch = d_batch.add(proof.masked_commitments[i].mul(s[i])).into_affine();
    }

    // Compute c2_batch
    let mut c2_batch = Gt::zero();

    for i in 0..64 {
        c2_batch = c2_batch.add(proof.ciphertexts[i].1.clone());
    }    

    // Verify sigma proof
    if local_verify_sigma_proof(
        &proof.card_commitment, 
        &d_batch, 
        &G1::generator(), 
        &c1, 
        &e_batch, 
        &c2_batch, 
        proof.sigma_proof.as_ref().unwrap()) == false {
        return false;
    }

    true
}

/// Verify that sigma proof is correctly verified by local_verify_sigma_proof
pub async fn test_sigma(evaluator: &mut Evaluator) {
    println!("Running test on sigma prove and verify...");

    let mut wit_1_handles = vec![];
    let mut lin_comb_ran = vec![];
    let wit_2_handle = evaluator.ran();

    for _ in 0..64 {
        wit_1_handles.push(evaluator.ran());
        lin_comb_ran.push(F::rand(&mut ark_std::test_rng()));
    }

    let mut d_i = vec![];
    let mut d_batch = G1::zero();

    for i in 0..64 {
        d_i.push(evaluator.exp_and_reveal_g1(vec![G1::generator()], vec![wit_1_handles[i].clone()], &format!("{}/{}", "test_D_", i)).await);
        d_batch = d_batch.add(d_i[i].mul(lin_comb_ran[i].clone())).into_affine();
    }

    let c_1 = evaluator.exp_and_reveal_g1(vec![G1::generator()], vec![wit_2_handle.clone()], &String::from("test_c_1")).await;

    let mut e_batch = Gt::zero();
    let e = Gt::generator();

    let mut c2_i = vec![];
    let mut c2_batch = Gt::zero();

    for i in 0..64 {
        e_batch = e_batch.add(e.mul(lin_comb_ran[i]));
        let tmp = evaluator.exp_and_reveal_gt(vec![Gt::generator()], vec![wit_1_handles[i].clone()], &format!("{}/{}", "test_c2_", i)).await;
        c2_i.push(tmp.add(evaluator.exp_and_reveal_gt(vec![Gt::generator()], vec![wit_2_handle.clone()], &format!("{}/{}", "test_e_", i)).await));

        c2_batch = c2_batch.add(c2_i[i].mul(lin_comb_ran[i].clone()));
    }

    let pi = dist_sigma_proof(
            evaluator,
            &G1::generator(), 
            &G1::generator(), 
            &e_batch, 
            wit_1_handles.clone(), 
            wit_2_handle.clone(), 
            lin_comb_ran.clone()).await;

    let check = local_verify_sigma_proof(
        &G1::generator(), 
        &d_batch, 
        &G1::generator(), 
        &c_1, 
        &e_batch, 
        &c2_batch, 
        &pi);
        
    assert!(check == true, "Verification failed");

    println!("Sigma proof test passed!");

}

pub fn test_local_kzg() {
    println!("Running test on local kzg...");

    let mut rng = ark_std::test_rng();
    let mut evals = vec![];

    let point: F = F::rand(&mut rng);

    for _ in 0..64 {
        let tmp = F::rand(&mut rng);
        evals.push(tmp);
    }

    let poly = utils::interpolate_poly_over_mult_subgroup(&evals);

    let divisor = DensePolynomial::from_coefficients_vec(vec![-point, F::from(1)]);

    // Divide by (X-z)
    let (quotient, _remainder) = 
        DenseOrSparsePolynomial::divide_with_q_and_r(
            &(&poly).into(),
            &(&divisor).into(),
        ).unwrap();

    let pi_poly = utils::commit_poly(&quotient);
    let com = utils::commit_poly(&poly);

    let poly_eval = poly.evaluate(&point);

    let b = utils::kzg_check(&com, &point, &poly_eval, &pi_poly);

    assert!(b == true, "Verification failed");
    
    println!("...Local KZG test passed!");
}

pub async fn test_dist_kzg(evaluator: &mut Evaluator) {
    println!("Running test on distributed kzg...");

    let mut evals = vec![];
    // let mut actual_evals = vec![];

    for _ in 0..64 {
        let tmp = evaluator.ran();
        evals.push(evaluator.get_wire(&tmp));
        // actual_evals.push(evaluator.output_wire(&tmp).await);
    }

    // let actual_poly = utils::interpolate_poly_over_mult_subgroup(&actual_evals);
    // let actual_evaluation_at_w = evaluator.share_poly_eval(actual_poly.clone(), utils::multiplicative_subgroup_of_size(64)).await;

    let poly = utils::interpolate_poly_over_mult_subgroup(&evals);
    let com_share = utils::commit_poly(&poly);
    let com = evaluator.add_g1_elements_from_all_parties(&com_share, &String::from("kzg_test_com")).await;

    let w = utils::multiplicative_subgroup_of_size(64);
    let pi = evaluator.eval_proof_with_share_poly(poly.clone(), w, String::from("kzg_test_pi")).await;

    let evaluation_at_w = evaluator.share_poly_eval(poly.clone(), w).await;


    let b = utils::kzg_check(&com, &w, &evaluator.output_wire(&evaluation_at_w).await, &pi);
    assert!(b == true, "Verification failed");

    println!("...Distributed KZG test passed!");
}

async fn test_share_poly_mult(evaluator: &mut Evaluator) {
    println!("Running test on share poly mult...");

    let mut share_evals_1 = vec![];
    let mut share_evals_2 = vec![];

    for _ in 0..64 {
        let tmp = evaluator.ran();
        share_evals_1.push(evaluator.get_wire(&tmp));
        let tmp = evaluator.ran();
        share_evals_2.push(evaluator.get_wire(&tmp));
    }

    let share_poly_1 = utils::interpolate_poly_over_mult_subgroup(&share_evals_1);
    let share_poly_2 = utils::interpolate_poly_over_mult_subgroup(&share_evals_2);

    let random_point = F::from(420021312);

    let share_poly_3 = evaluator.share_poly_mult(
        share_poly_1.clone(), 
        share_poly_2.clone()
    ).await;

    // Evaluate share_poly_1, share_poly_2 and share_poly_3 at random_point
    let poly_1_val = evaluator.share_poly_eval(share_poly_1.clone(), random_point).await;
    let poly_2_val = evaluator.share_poly_eval(share_poly_2.clone(), random_point).await;
    let poly_3_val = evaluator.share_poly_eval(share_poly_3.clone(), random_point).await;

    let v_1 = evaluator.output_wire(&poly_1_val).await;
    let v_2 = evaluator.output_wire(&poly_2_val).await;
    let v_3 = evaluator.output_wire(&poly_3_val).await;

    assert_eq!(v_1 * v_2, v_3, "Share poly mult failed");
    
    println!("...Share poly mult test passed!");
}