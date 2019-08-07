use blake2s_simd::Params as Blake2s;
use ff::PrimeField;
use paired::bls12_381::Fr;

use crate::fr32::bytes_into_fr_repr_safe;

/// Key derivation function, based on pedersen hashing.
pub fn kdf(data: &[u8]) -> Fr {
    let hash = Blake2s::new()
        .hash_length(32)
        .to_state()
        .update(data)
        .finalize();

    Fr::from_repr(bytes_into_fr_repr_safe(hash.as_ref())).unwrap()
}

#[cfg(test)]
mod tests {
    use super::kdf;
    use crate::fr32::bytes_into_fr;
    use paired::bls12_381::Bls12;

    #[test]
    fn kdf_valid_block_len() {
        let m = 1;
        let size = 32 * (1 + m);

        let data = vec![1u8; size];
        let expected = bytes_into_fr::<Bls12>(
            &mut vec![
                220, 60, 76, 126, 119, 247, 67, 162, 98, 94, 119, 28, 247, 18, 71, 208, 167, 72,
                33, 85, 59, 56, 96, 13, 9, 67, 49, 109, 95, 246, 152, 63,
            ]
            .as_slice(),
        )
        .unwrap();

        let res = kdf(&data);
        assert_eq!(res, expected);
    }
}
