use super::{
    compute_evaluation_vector, make_schema, ProofBuilder, ProofCounts, ProvableQueryResult,
    QueryExpr, QueryResult, SumcheckMleEvaluations, SumcheckRandomScalars, VerificationBuilder,
};

use crate::base::{
    database::{CommitmentAccessor, DataAccessor},
    polynomial::CompositePolynomialInfo,
    proof::{MessageLabel, ProofError, TranscriptProtocol},
};
use crate::proof_primitive::{inner_product::InnerProductProof, sumcheck::SumcheckProof};

use bumpalo::Bump;
use byte_slice_cast::AsByteSlice;
use curve25519_dalek::{
    ristretto::{CompressedRistretto, RistrettoPoint},
    scalar::Scalar,
    traits::Identity,
};
use merlin::Transcript;
use pedersen::compute::get_generators;
use serde::{Deserialize, Serialize};

/// The proof for a query.
///
/// Note: Because the class is deserialized from untrusted data, it
/// cannot maintain any invariant on its data members; hence, they are
/// all public so as to allow for easy manipulation for testing.
#[derive(Clone, Serialize, Deserialize)]
pub struct QueryProof {
    pub commitments: Vec<CompressedRistretto>,
    pub sumcheck_proof: SumcheckProof,
    pub pre_result_mle_evaluations: Vec<Scalar>,
    pub evaluation_proof: InnerProductProof,
}

impl QueryProof {
    pub fn new(
        expr: &dyn QueryExpr,
        accessor: &dyn DataAccessor,
        counts: &ProofCounts,
    ) -> (Self, ProvableQueryResult) {
        assert!(counts.sumcheck_variables > 0);
        let n = 1 << counts.sumcheck_variables;
        let alloc = Bump::new();

        // pass over provable AST to fill in the proof builder
        let mut builder = ProofBuilder::new(counts);
        expr.prover_evaluate(&mut builder, &alloc, accessor);

        // commit to any intermediate MLEs
        let commitments = builder.commit_intermediate_mles();

        // compute the query's result
        let provable_result = builder.make_provable_query_result();

        // construct a transcript for the proof
        let mut transcript = make_transcript(
            &commitments,
            &provable_result.indexes,
            &provable_result.data,
        );

        // construct the sumcheck polynomial
        let mut random_scalars = vec![Scalar::zero(); SumcheckRandomScalars::count(counts)];
        transcript.challenge_scalars(&mut random_scalars, MessageLabel::QuerySumcheckChallenge);
        let poly =
            builder.make_sumcheck_polynomial(&SumcheckRandomScalars::new(counts, &random_scalars));

        // create the sumcheck proof -- this is the main part of proving a query
        let mut evaluation_point = vec![Scalar::zero(); poly.num_variables];
        let sumcheck_proof = SumcheckProof::create(&mut transcript, &mut evaluation_point, &poly);

        // evaluate the MLEs used in sumcheck except for the result columns
        let evaluation_vec = compute_evaluation_vector(&evaluation_point);
        let pre_result_mle_evaluations = builder.evaluate_pre_result_mles(&evaluation_vec);

        // commit to the MLE evaluations
        transcript.append_scalars(
            MessageLabel::QueryMleEvaluations,
            &pre_result_mle_evaluations,
        );

        // fold together the pre result MLEs -- this will form the input to an inner product proof
        // of their evaluations (fold in this context means create a random linear combination)
        let mut random_scalars = vec![Scalar::zero(); pre_result_mle_evaluations.len()];
        transcript.challenge_scalars(
            &mut random_scalars,
            MessageLabel::QueryMleEvaluationsChallenge,
        );
        let folded_mle = builder.fold_pre_result_mles(&random_scalars);

        // finally, form the inner product proof of the MLEs' evaluations
        let mut generators = vec![RistrettoPoint::identity(); n + 1];
        get_generators(&mut generators, 0);
        let product_g = generators[n];
        let evaluation_proof = InnerProductProof::create(
            &mut transcript,
            &product_g,
            &generators[..n],
            &folded_mle,
            &evaluation_vec,
        );

        let proof = Self {
            commitments,
            sumcheck_proof,
            pre_result_mle_evaluations,
            evaluation_proof,
        };
        (proof, provable_result)
    }

    pub fn verify(
        &self,
        expr: &dyn QueryExpr,
        accessor: &impl CommitmentAccessor,
        counts: &ProofCounts,
        result: &ProvableQueryResult,
    ) -> Result<QueryResult, ProofError> {
        assert!(counts.sumcheck_variables > 0);
        let n = 1 << counts.sumcheck_variables;

        // verify sizes
        if !self.validate_sizes(counts, result) {
            return Err(ProofError::VerificationError);
        }

        // decompress commitments
        let mut commitments = Vec::with_capacity(self.commitments.len());
        for commitment in self.commitments.iter() {
            if let Some(commitment) = commitment.decompress() {
                commitments.push(commitment);
            } else {
                return Err(ProofError::VerificationError);
            }
        }

        // construct a transcript for the proof
        let mut transcript = make_transcript(&self.commitments, &result.indexes, &result.data);

        // draw the random scalars for sumcheck
        let mut random_scalars = vec![Scalar::zero(); SumcheckRandomScalars::count(counts)];
        transcript.challenge_scalars(&mut random_scalars, MessageLabel::QuerySumcheckChallenge);
        let sumcheck_random_scalars = SumcheckRandomScalars::new(counts, &random_scalars);

        // verify sumcheck up to the evaluation check
        let poly_info = CompositePolynomialInfo {
            max_multiplicands: counts.sumcheck_max_multiplicands,
            num_variables: counts.sumcheck_variables,
        };
        let subclaim = self.sumcheck_proof.verify_without_evaluation(
            &mut transcript,
            poly_info,
            &Scalar::zero(),
        )?;
        let evaluation_vec = compute_evaluation_vector(&subclaim.evaluation_point);

        // commit to mle evaluations
        transcript.append_scalars(
            MessageLabel::QueryMleEvaluations,
            &self.pre_result_mle_evaluations,
        );

        // draw the random scalars for the evaluation proof
        // (i.e. the folding/random linear combination of the pre_result_mles)
        let mut evaluation_random_scalars =
            vec![Scalar::zero(); self.pre_result_mle_evaluations.len()];
        transcript.challenge_scalars(
            &mut evaluation_random_scalars,
            MessageLabel::QueryMleEvaluationsChallenge,
        );

        // compute the evaluation of the result MLEs
        let result_evaluations = match result.evaluate(&evaluation_vec) {
            Some(evaluations) => evaluations,
            _ => return Err(ProofError::VerificationError),
        };

        // pass over the provable AST to fill in the verification builder
        let sumcheck_evaluations = SumcheckMleEvaluations::new(
            counts.table_length,
            &evaluation_vec,
            sumcheck_random_scalars.entrywise_multipliers,
            &self.pre_result_mle_evaluations,
            &result_evaluations,
        );
        let mut builder = VerificationBuilder::new(
            sumcheck_evaluations,
            &commitments,
            sumcheck_random_scalars.subpolynomial_multipliers,
            &evaluation_random_scalars,
        );
        expr.verifier_evaluate(&mut builder, accessor);

        // perform the evaluation check of the sumcheck polynomial
        if builder.sumcheck_evaluation() != subclaim.expected_evaluation {
            return Err(ProofError::VerificationError);
        }

        // finally, check the MLE evaluations with the inner product proof
        let mut generators = vec![RistrettoPoint::identity(); n + 1];
        get_generators(&mut generators, 0);
        let product_g = generators[n];
        let expected_commit = builder.folded_pre_result_commitment()
            + product_g * builder.folded_pre_result_evaluation();
        self.evaluation_proof.verify(
            &mut transcript,
            &expected_commit,
            &product_g,
            &generators[..n],
            &evaluation_vec,
        )?;

        Ok(result.into_query_result(make_schema(counts.result_columns)))
    }

    fn validate_sizes(&self, counts: &ProofCounts, result: &ProvableQueryResult) -> bool {
        result.num_columns as usize == counts.result_columns
            && self.commitments.len() == counts.intermediate_mles
            && self.pre_result_mle_evaluations.len()
                == counts.intermediate_mles + counts.anchored_mles
    }
}

fn make_transcript(
    commitments: &[CompressedRistretto],
    result_indexes: &[u64],
    result_data: &[u8],
) -> merlin::Transcript {
    let mut transcript = Transcript::new(MessageLabel::QueryProof.as_bytes());
    transcript.append_points(MessageLabel::QueryCommit, commitments);
    transcript.append_message(
        MessageLabel::QueryResultIndexes.as_bytes(),
        result_indexes.as_byte_slice(),
    );
    transcript.append_message(MessageLabel::QueryResultData.as_bytes(), result_data);
    transcript
}