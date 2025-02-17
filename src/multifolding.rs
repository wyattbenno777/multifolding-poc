use ark_ec::CurveGroup;
use ark_ff::Field;
use ark_std::{One, Zero};
use std::ops::Add;

use subroutines::PolyIOP;
use transcript::IOPTranscript;

use crate::ccs::cccs::{Witness, CCCS};
use crate::ccs::ccs::CCS;
use crate::ccs::lcccs::LCCCS;
use crate::ccs::util::compute_all_sum_Mz_evals;
use crate::espresso::sum_check::structs::IOPProof as SumCheckProof;
use crate::espresso::sum_check::{verifier::interpolate_uni_poly, SumCheck};
use crate::espresso::virtual_polynomial::{eq_eval, VPAuxInfo, VirtualPolynomial};
use crate::util::hypercube::BooleanHypercube;

use std::marker::PhantomData;

#[derive(Debug)]
pub struct Multifolding<C: CurveGroup> {
    pub _c: PhantomData<C>,
}

impl<C: CurveGroup> Multifolding<C> {
    /// Compute sigma_i and theta_i from step 4
    pub fn compute_sigmas_and_thetas(
        ccs: &CCS<C>,
        z1: &Vec<C::ScalarField>,
        z2: &Vec<C::ScalarField>,
        r_x_prime: &[C::ScalarField],
    ) -> (Vec<C::ScalarField>, Vec<C::ScalarField>) {
        (
            // sigmas
            compute_all_sum_Mz_evals(&ccs.M, z1, r_x_prime, ccs.s_prime),
            // thetas
            compute_all_sum_Mz_evals(&ccs.M, z2, r_x_prime, ccs.s_prime),
        )
    }

    /// Compute the right-hand-side of step 5 of the multifolding scheme
    pub fn compute_c_from_sigmas_and_thetas(
        ccs: &CCS<C>,
        sigmas: &[C::ScalarField],
        thetas: &[C::ScalarField],
        gamma: C::ScalarField,
        beta: &[C::ScalarField],
        r_x: &[C::ScalarField],
        r_x_prime: &[C::ScalarField],
    ) -> C::ScalarField {
        let mut c = C::ScalarField::zero();

        let e1 = eq_eval(r_x, r_x_prime).unwrap();
        let e2 = eq_eval(beta, r_x_prime).unwrap();

        // (sum gamma^j * e1 * sigma_j)
        for (j, sigma_j) in sigmas.iter().enumerate() {
            let gamma_j = gamma.pow([j as u64]);
            c += gamma_j * e1 * sigma_j;
        }

        // + gamma^{t+1} * e2 * sum c_i * prod theta_j
        let mut lhs = C::ScalarField::zero();
        for i in 0..ccs.q {
            let mut prod = C::ScalarField::one();
            for j in ccs.S[i].clone() {
                prod *= thetas[j];
            }
            lhs += ccs.c[i] * prod;
        }
        let gamma_t1 = gamma.pow([(ccs.t + 1) as u64]);
        c += gamma_t1 * e2 * lhs;
        c
    }

    /// Compute g(x) polynomial for the given inputs.
    pub fn compute_g(
        running_instance: &LCCCS<C>,
        cccs_instance: &CCCS<C>,
        z1: &Vec<C::ScalarField>,
        z2: &Vec<C::ScalarField>,
        gamma: C::ScalarField,
        beta: &[C::ScalarField],
    ) -> VirtualPolynomial<C::ScalarField> {
        let mut vec_L = running_instance.compute_Ls(z1);
        let mut Q = cccs_instance.compute_Q(z2, beta);
        let mut g = vec_L[0].clone();
        for (j, L_j) in vec_L.iter_mut().enumerate().skip(1) {
            let gamma_j = gamma.pow([j as u64]);
            L_j.scalar_mul(&gamma_j);
            g = g.add(L_j);
        }
        let gamma_t1 = gamma.pow([(cccs_instance.ccs.t + 1) as u64]);
        Q.scalar_mul(&gamma_t1);
        g = g.add(&Q);
        g
    }

    /// Perform the multifolding prover.
    ///
    /// Given an LCCCS instance and a CCS instance, fold them into a single LCCCS instance.
    /// Since this is the prover, also fold their witness.
    ///
    /// Return the final folded LCCCS, the folded witness, the sumcheck proof, and the helper sumcheck claims sigmas
    /// and thetas.
    pub fn prove(
        transcript: &mut IOPTranscript<C::ScalarField>,
        running_instance: &LCCCS<C>,
        new_instance: &CCCS<C>,
        w_1: &Witness<C::ScalarField>,
        w_2: &Witness<C::ScalarField>,
    ) -> (
        SumCheckProof<C::ScalarField>,
        Vec<C::ScalarField>,
        Vec<C::ScalarField>,
        LCCCS<C>,
        Witness<C::ScalarField>,
    ) {
        // TODO appends to transcript

        // construct the LCCCS z vector from the relaxation factor, public IO and witness
        // XXX this deserves its own function in LCCCS
        let z_1: Vec<C::ScalarField> = [
            vec![running_instance.u],
            running_instance.x.clone(),
            w_1.w.to_vec(),
        ]
        .concat();
        // construct the CCCS z vector from the public IO and witness
        let z_2: Vec<C::ScalarField> = [
            vec![C::ScalarField::one()],
            new_instance.x.clone(),
            w_2.w.to_vec(),
        ]
        .concat();

        // Step 1: Get some challenges
        let gamma: C::ScalarField = transcript.get_and_append_challenge(b"gamma").unwrap();
        let beta: Vec<C::ScalarField> = transcript
            .get_and_append_challenge_vectors(b"beta", running_instance.ccs.s)
            .unwrap();

        // Compute g(x)
        let g = Self::compute_g(
            running_instance,
            new_instance,
            &z_1,
            &z_2,
            gamma,
            &beta,
        );

        // Step 3: Run the sumcheck prover
        let sumcheck_proof =
            <PolyIOP<C::ScalarField> as SumCheck<C::ScalarField>>::prove(&g, transcript).unwrap(); // XXX unwrap

        // Note: The following two "sanity checks" are done for this prototype, in a final version
        // they should be removed.
        //
        // Sanity check 1: evaluate g(x) over x \in {0,1} (the boolean hypercube), and check that
        // its sum is equal to the extracted_sum from the SumCheck.
        //////////////////////////////////////////////////////////////////////
        let mut g_over_bhc = C::ScalarField::zero();
        for x in BooleanHypercube::new(running_instance.ccs.s) {
            g_over_bhc += g.evaluate(&x).unwrap();
        }

        // note: this is the sum of g(x) over the whole boolean hypercube
        let extracted_sum =
            <PolyIOP<C::ScalarField> as SumCheck<C::ScalarField>>::extract_sum(&sumcheck_proof);
        assert_eq!(extracted_sum, g_over_bhc);
        // Sanity check 2: expect \sum v_j * gamma^j to be equal to the sum of g(x) over the
        // boolean hypercube (and also equal to the extracted_sum from the SumCheck).
        let mut sum_v_j_gamma = C::ScalarField::zero();
        for j in 0..running_instance.v.len() {
            let gamma_j = gamma.pow([j as u64]);
            sum_v_j_gamma += running_instance.v[j] * gamma_j;
        }
        assert_eq!(g_over_bhc, sum_v_j_gamma);
        assert_eq!(extracted_sum, sum_v_j_gamma);
        //////////////////////////////////////////////////////////////////////

        // Step 2: dig into the sumcheck and extract r_x_prime
        let r_x_prime = sumcheck_proof.point.clone();

        // Step 4: compute sigmas and thetas
        let (sigmas, thetas) =
            Self::compute_sigmas_and_thetas(&running_instance.ccs, &z_1, &z_2, &r_x_prime);

        // Step 6: Get the folding challenge
        let rho: C::ScalarField = transcript.get_and_append_challenge(b"rho").unwrap();

        // Step 7: Create the folded instance
        let folded_lcccs = LCCCS::fold(
            running_instance,
            new_instance,
            &sigmas,
            &thetas,
            r_x_prime,
            rho,
        );

        // Step 8: Fold the witnesses
        let folded_witness = LCCCS::<C>::fold_witness(w_1, w_2, rho);

        (sumcheck_proof, sigmas, thetas, folded_lcccs, folded_witness)
    }

    /// Perform the multifolding verifier:
    ///
    /// Given an LCCCS instance and a CCS instance, fold them into a single LCCCS instance.
    ///
    /// Return the folded LCCCS instance.
    pub fn verify(
        transcript: &mut IOPTranscript<C::ScalarField>,
        running_instance: &LCCCS<C>,
        new_instance: &CCCS<C>,
        proof: SumCheckProof<C::ScalarField>,
        sigmas: &[C::ScalarField],
        thetas: &[C::ScalarField],
    ) -> LCCCS<C> {
        // TODO appends to transcript

        // Step 1: Get some challenges
        let gamma: C::ScalarField = transcript.get_and_append_challenge(b"gamma").unwrap();
        let beta: Vec<C::ScalarField> = transcript
            .get_and_append_challenge_vectors(b"beta", running_instance.ccs.s)
            .unwrap();

        let vp_aux_info = VPAuxInfo::<C::ScalarField> {
            max_degree: running_instance.ccs.d + 1,
            num_variables: running_instance.ccs.s,
            phantom: PhantomData::<C::ScalarField>,
        };

        // Step 3: Start verifying the sumcheck
        // First, compute the expected sumcheck sum: \sum gamma^j v_j
        let mut sum_v_j_gamma = C::ScalarField::zero();
        for j in 0..running_instance.v.len() {
            let gamma_j = gamma.pow([j as u64]);
            sum_v_j_gamma += running_instance.v[j] * gamma_j;
        }

        // Verify the interactive part of the sumcheck
        let sumcheck_subclaim = <PolyIOP<C::ScalarField> as SumCheck<C::ScalarField>>::verify(
            sum_v_j_gamma,
            &proof,
            &vp_aux_info,
            transcript,
        )
        .unwrap();

        // Step 2: Dig into the sumcheck claim and extract the randomness used
        let r_x_prime = sumcheck_subclaim.point.clone();

        // Step 5: Finish verifying sumcheck (verify the claim c)
        let c = Self::compute_c_from_sigmas_and_thetas(
            &new_instance.ccs,
            sigmas,
            thetas,
            gamma,
            &beta,
            &running_instance.r_x,
            &r_x_prime,
        );
        // check that the g(r_x') from the sumcheck proof is equal to the computed c from sigmas&thetas
        assert_eq!(c, sumcheck_subclaim.expected_evaluation);

        // Sanity check: we can also compute g(r_x') from the proof last evaluation value, and
        // should be equal to the previously obtained values.
        let g_on_rxprime_from_sumcheck_last_eval = interpolate_uni_poly::<C::ScalarField>(
            &proof.proofs.last().unwrap().evaluations,
            *r_x_prime.last().unwrap(),
        )
        .unwrap();
        assert_eq!(g_on_rxprime_from_sumcheck_last_eval, c);
        assert_eq!(
            g_on_rxprime_from_sumcheck_last_eval,
            sumcheck_subclaim.expected_evaluation
        );

        // Step 6: Get the folding challenge
        let rho: C::ScalarField = transcript.get_and_append_challenge(b"rho").unwrap();

        // Step 7: Compute the folded instance
        LCCCS::fold(
            running_instance,
            new_instance,
            sigmas,
            thetas,
            r_x_prime,
            rho,
        )
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::ccs::ccs::test::{get_test_ccs, get_test_z};
    use ark_std::test_rng;
    use ark_std::UniformRand;

    use crate::ccs::pedersen::Pedersen;
    use ark_bls12_381::{Fr, G1Projective};

    // NIMFS: Non Interactive Multifolding Scheme
    type NIMFS = Multifolding<G1Projective>;

    #[test]
    fn test_compute_sigmas_and_thetas() -> () {
        let ccs = get_test_ccs();
        let z1 = get_test_z(3);
        let z2 = get_test_z(4);
        ccs.check_relation(&z1).unwrap();
        ccs.check_relation(&z2).unwrap();

        let mut rng = test_rng();
        let gamma: Fr = Fr::rand(&mut rng);
        let beta: Vec<Fr> = (0..ccs.s).map(|_| Fr::rand(&mut rng)).collect();
        let r_x_prime: Vec<Fr> = (0..ccs.s).map(|_| Fr::rand(&mut rng)).collect();

        // Initialize a multifolding object
        let pedersen_params = Pedersen::new_params(&mut rng, ccs.n - ccs.l - 1);
        let (lcccs_instance, _) = ccs.to_lcccs(&mut rng, &pedersen_params, &z1);
        let (cccs_instance, _) = ccs.to_cccs(&mut rng, &pedersen_params, &z2);

        let (sigmas, thetas) =
            NIMFS::compute_sigmas_and_thetas(&lcccs_instance.ccs, &z1, &z2, &r_x_prime);

        let g = NIMFS::compute_g(
            &lcccs_instance,
            &cccs_instance,
            &z1,
            &z2,
            gamma,
            &beta,
        );

        // we expect g(r_x_prime) to be equal to:
        // c = (sum gamma^j * e1 * sigma_j) + gamma^{t+1} * e2 * sum c_i * prod theta_j
        // from compute_c_from_sigmas_and_thetas
        let expected_c = g.evaluate(&r_x_prime).unwrap();
        let c = NIMFS::compute_c_from_sigmas_and_thetas(
            &ccs, &sigmas, &thetas, gamma, &beta, &lcccs_instance.r_x, &r_x_prime,
        );
        assert_eq!(c, expected_c);
    }

    #[test]
    fn test_compute_g() -> () {
        let ccs = get_test_ccs();
        let z1 = get_test_z(3);
        let z2 = get_test_z(4);
        ccs.check_relation(&z1).unwrap();
        ccs.check_relation(&z2).unwrap();

        let mut rng = test_rng(); // TMP
        let gamma: Fr = Fr::rand(&mut rng);
        let beta: Vec<Fr> = (0..ccs.s).map(|_| Fr::rand(&mut rng)).collect();

        // Initialize a multifolding object
        let pedersen_params = Pedersen::new_params(&mut rng, ccs.n - ccs.l - 1);
        let (lcccs_instance, _) = ccs.to_lcccs(&mut rng, &pedersen_params, &z1);
        let (cccs_instance, _) = ccs.to_cccs(&mut rng, &pedersen_params, &z2);

        let mut sum_v_j_gamma = Fr::zero();
        for j in 0..lcccs_instance.v.len() {
            let gamma_j = gamma.pow([j as u64]);
            sum_v_j_gamma += lcccs_instance.v[j] * gamma_j;
        }

        // Compute g(x) with that r_x
        let g = NIMFS::compute_g(
            &lcccs_instance,
            &cccs_instance,
            &z1,
            &z2,
            gamma,
            &beta,
        );

        // evaluate g(x) over x \in {0,1}^s
        let mut g_on_bhc = Fr::zero();
        for x in BooleanHypercube::new(ccs.s).into_iter() {
            g_on_bhc += g.evaluate(&x).unwrap();
        }

        // evaluate sum_{j \in [t]} (gamma^j * Lj(x)) over x \in {0,1}^s
        let mut sum_Lj_on_bhc = Fr::zero();
        let vec_L = lcccs_instance.compute_Ls(&z1);
        for x in BooleanHypercube::new(ccs.s).into_iter() {
            for j in 0..vec_L.len() {
                let gamma_j = gamma.pow([j as u64]);
                sum_Lj_on_bhc += vec_L[j].evaluate(&x).unwrap() * gamma_j;
            }
        }

        // Q(x) over bhc is assumed to be zero, as checked in the test 'test_compute_Q'
        assert_ne!(g_on_bhc, Fr::zero());

        // evaluating g(x) over the boolean hypercube should give the same result as evaluating the
        // sum of gamma^j * Lj(x) over the boolean hypercube
        assert_eq!(g_on_bhc, sum_Lj_on_bhc);

        // evaluating g(x) over the boolean hypercube should give the same result as evaluating the
        // sum of gamma^j * v_j over j \in [t]
        assert_eq!(g_on_bhc, sum_v_j_gamma);
    }

    #[test]
    pub fn test_multifolding() {
        let mut rng = test_rng();

        // Create a basic CCS circuit
        let ccs = get_test_ccs::<G1Projective>();
        let pedersen_params = Pedersen::new_params(&mut rng, ccs.n - ccs.l - 1);

        // Generate a satisfying witness
        let z_1 = get_test_z(3);
        // Generate another satisfying witness
        let z_2 = get_test_z(4);

        // Create the LCCCS instance out of z_1
        let (running_instance, w1) = ccs.to_lcccs(&mut rng, &pedersen_params, &z_1);
        // Create the CCCS instance out of z_2
        let (new_instance, w2) = ccs.to_cccs(&mut rng, &pedersen_params, &z_2);

        // Prover's transcript
        let mut transcript_p = IOPTranscript::<Fr>::new(b"multifolding");
        transcript_p.append_message(b"init", b"init").unwrap();

        // Run the prover side of the multifolding
        let (sumcheck_proof, sigmas, thetas, folded_lcccs, folded_witness) = NIMFS::prove(
            &mut transcript_p,
            &running_instance,
            &new_instance,
            &w1,
            &w2,
        );

        // Verifier's transcript
        let mut transcript_v = IOPTranscript::<Fr>::new(b"multifolding");
        transcript_v.append_message(b"init", b"init").unwrap();

        // Run the verifier side of the multifolding
        let folded_lcccs_v = NIMFS::verify(
            &mut transcript_v,
            &running_instance,
            &new_instance,
            sumcheck_proof,
            &sigmas,
            &thetas,
        );
        assert_eq!(folded_lcccs, folded_lcccs_v);

        // Check that the folded LCCCS instance is a valid instance with respect to the folded witness
        folded_lcccs
            .check_relation(&pedersen_params, &folded_witness)
            .unwrap();
    }

    #[test]
    pub fn test_multifolding_multiple_steps() {
        let mut rng = test_rng();

        let ccs = get_test_ccs::<G1Projective>();

        let pedersen_params = Pedersen::new_params(&mut rng, ccs.n - ccs.l - 1);

        // LCCCS witness
        let z_1 = get_test_z(2);
        let (mut running_instance, mut w1) = ccs.to_lcccs(&mut rng, &pedersen_params, &z_1);

        let mut transcript_p = IOPTranscript::<Fr>::new(b"multifolding");
        let mut transcript_v = IOPTranscript::<Fr>::new(b"multifolding");
        transcript_p.append_message(b"init", b"init").unwrap();
        transcript_v.append_message(b"init", b"init").unwrap();

        let n: usize = 10;
        for i in 3..n {
            println!("\niteration: i {}", i); // DBG

            // CCS witness
            let z_2 = get_test_z(i);
            println!("z_2 {:?}", z_2); // DBG

            let (new_instance, w2) = ccs.to_cccs(&mut rng, &pedersen_params, &z_2);

            // run the prover side of the multifolding
            let (sumcheck_proof, sigmas, thetas, folded_lcccs, folded_witness) = NIMFS::prove(
                &mut transcript_p,
                &running_instance,
                &new_instance,
                &w1,
                &w2,
            );

            // run the verifier side of the multifolding
            let folded_lcccs_v = NIMFS::verify(
                &mut transcript_v,
                &running_instance,
                &new_instance,
                sumcheck_proof,
                &sigmas,
                &thetas,
            );

            assert_eq!(folded_lcccs, folded_lcccs_v);

            // check that the folded instance with the folded witness holds the LCCCS relation
            println!("check_relation {}", i);
            folded_lcccs
                .check_relation(&pedersen_params, &folded_witness)
                .unwrap();

            running_instance = folded_lcccs;
            w1 = folded_witness;
        }
    }
}
