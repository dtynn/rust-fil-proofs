//! ZigZagDrgPorep is a layered PoRep which replicates layer by layer.
//! Between layers, the graph is 'reversed' in such a way that the dependencies expand with each iteration.
//! This reversal is not a straightforward inversion -- so we coin the term 'zigzag' to describe the transformation.
//! Each graph can be divided into base and expansion components.
//! The 'base' component is an ordinary DRG. The expansion component attempts to add a target (expansion_degree) number of connections
//! between nodes in a reversible way. Expansion connections are therefore simply inverted at each layer.
//! Because of how DRG-sampled parents are calculated on demand, the base components are not. Instead, a same-degree
//! DRG with connections in the opposite direction (and using the same random seed) is used when calculating parents on demand.
//! For the algorithm to have the desired properties, it is important that the expansion components are directly inverted at each layer.
//! However, it is fortunately not necessary that the base DRG components also have this property.

use std::marker::PhantomData;

use rayon::prelude::*;
use serde::de::Deserialize;
use serde::ser::Serialize;

use crate::drgporep::{self, DrgPoRep};
use crate::drgraph::Graph;
use crate::error::Result;
use crate::hasher::{Domain, HashFunction, Hasher};
use crate::merkle::{next_pow2, populate_leaves, MerkleProof, MerkleStore, MerkleTree, Store};
use crate::parameter_cache::ParameterSetMetadata;
use crate::porep::PoRep;
use crate::proof::ProofScheme;
use crate::util::{data_at_node, NODE_SIZE};
use crate::vde;
use crate::zigzag::column::Column;
use crate::zigzag::column_proof::ColumnProof;
use crate::zigzag::encoding_proof::EncodingProof;
use crate::zigzag::hash::hash2;
use crate::zigzag::{ChallengeRequirements, LayerChallenges, ZigZagBucketGraph};

type Tree<H> = MerkleTree<<H as Hasher>::Domain, <H as Hasher>::Function>;

#[derive(Debug)]
pub struct ZigZagDrgPoRep<'a, H: 'a + Hasher> {
    _a: PhantomData<&'a H>,
}

#[derive(Debug)]
pub struct SetupParams {
    pub drg: drgporep::DrgParams,
    pub layer_challenges: LayerChallenges,
}

#[derive(Debug, Clone)]
pub struct PublicParams<H, G>
where
    H: Hasher,
    G: Graph<H> + ParameterSetMetadata,
{
    pub graph: G,
    pub layer_challenges: LayerChallenges,
    _h: PhantomData<H>,
}

impl<H, G> PublicParams<H, G>
where
    H: Hasher,
    G: Graph<H> + ParameterSetMetadata,
{
    pub fn new(graph: G, layer_challenges: LayerChallenges) -> Self {
        PublicParams {
            graph,
            layer_challenges,
            _h: PhantomData,
        }
    }
}

impl<H, G> ParameterSetMetadata for PublicParams<H, G>
where
    H: Hasher,
    G: Graph<H> + ParameterSetMetadata,
{
    fn identifier(&self) -> String {
        format!(
            "layered_drgporep::PublicParams{{ graph: {}, challenges: {:?} }}",
            self.graph.identifier(),
            self.layer_challenges,
        )
    }

    fn sector_size(&self) -> u64 {
        self.graph.sector_size()
    }
}

impl<'a, H, G> From<&'a PublicParams<H, G>> for PublicParams<H, G>
where
    H: Hasher,
    G: Graph<H> + ParameterSetMetadata,
{
    fn from(other: &PublicParams<H, G>) -> PublicParams<H, G> {
        PublicParams::new(other.graph.clone(), other.layer_challenges.clone())
    }
}

#[derive(Debug, Clone)]
pub struct PublicInputs<T: Domain> {
    pub replica_id: T,
    pub seed: Option<T>,
    pub tau: Tau<T>,
    pub k: Option<usize>,
}

impl<T: Domain> PublicInputs<T> {
    pub fn challenges(
        &self,
        layer_challenges: &LayerChallenges,
        leaves: usize,
        partition_k: Option<usize>,
    ) -> Vec<usize> {
        let k = partition_k.unwrap_or(0);

        if let Some(ref seed) = self.seed {
            layer_challenges.derive::<T>(leaves, &self.replica_id, seed, k as u8)
        } else {
            layer_challenges.derive::<T>(leaves, &self.replica_id, &self.tau.comm_r, k as u8)
        }
    }
}

pub struct PrivateInputs<H: Hasher> {
    pub p_aux: PersistentAux<H::Domain>,
    pub t_aux: TemporaryAux<H>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proof<H: Hasher> {
    #[serde(bound(
        serialize = "MerkleProof<H>: Serialize",
        deserialize = "MerkleProof<H>: Deserialize<'de>"
    ))]
    pub comm_d_proofs: Vec<MerkleProof<H>>,
    #[serde(bound(
        serialize = "MerkleProof<H>: Serialize, ColumnProof<H>: Serialize",
        deserialize = "MerkleProof<H>: Deserialize<'de>, ColumnProof<H>: Deserialize<'de>"
    ))]
    pub comm_r_last_proofs: Vec<(MerkleProof<H>, Vec<MerkleProof<H>>)>,
    #[serde(bound(
        serialize = "ReplicaColumnProof<H>: Serialize",
        deserialize = "ReplicaColumnProof<H>: Deserialize<'de>"
    ))]
    pub replica_column_proofs: Vec<ReplicaColumnProof<H>>,
    #[serde(bound(
        serialize = "EncodingProof<H>: Serialize",
        deserialize = "EncodingProof<H>: Deserialize<'de>"
    ))]
    /// Indexed by challenge. The encoding proof for layer 1.
    pub encoding_proof_1: Vec<EncodingProof<H>>,
    #[serde(bound(
        serialize = "EncodingProof<H>: Serialize",
        deserialize = "EncodingProof<H>: Deserialize<'de>"
    ))]
    /// Indexed first by challenge then by layer in 2..layers - 1.
    pub encoding_proofs: Vec<Vec<EncodingProof<H>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaColumnProof<H: Hasher> {
    #[serde(bound(
        serialize = "ColumnProof<H>: Serialize",
        deserialize = "ColumnProof<H>: Deserialize<'de>"
    ))]
    c_x: ColumnProof<H>,
    #[serde(bound(
        serialize = "ColumnProof<H>: Serialize",
        deserialize = "ColumnProof<H>: Deserialize<'de>"
    ))]
    c_inv_x: ColumnProof<H>,
    #[serde(bound(
        serialize = "ColumnProof<H>: Serialize",
        deserialize = "ColumnProof<H>: Deserialize<'de>"
    ))]
    drg_parents: Vec<ColumnProof<H>>,
    #[serde(bound(
        serialize = "ColumnProof<H>: Serialize",
        deserialize = "ColumnProof<H>: Deserialize<'de>"
    ))]
    exp_parents_even: Vec<ColumnProof<H>>,
    #[serde(bound(
        serialize = "ColumnProof<H>: Serialize",
        deserialize = "ColumnProof<H>: Deserialize<'de>"
    ))]
    exp_parents_odd: Vec<ColumnProof<H>>,
}

impl<H: Hasher> Proof<H> {
    pub fn serialize(&self) -> Vec<u8> {
        unimplemented!();
    }
}

type TransformedLayers<H> = (
    Tau<<H as Hasher>::Domain>,
    PersistentAux<<H as Hasher>::Domain>,
    TemporaryAux<H>,
);

/// Tau for a single parition.
#[derive(Debug, Clone)]
pub struct Tau<D: Domain> {
    comm_d: D,
    comm_r: D,
}

#[derive(Debug, Clone)]
/// Stored along side the sector on disk.
pub struct PersistentAux<D: Domain> {
    comm_c: D,
    comm_r_last: D,
}

#[derive(Debug, Clone)]
pub struct TemporaryAux<H: Hasher> {
    /// The encoded nodes for 1..layers.
    encodings: Vec<Vec<u8>>,
    tree_d: Tree<H>,
    tree_r_last: Tree<H>,
    tree_c: Tree<H>,
    /// E_i
    es: Vec<Vec<u8>>,
    /// O_i
    os: Vec<Vec<u8>>,
}

fn get_node<H: Hasher>(data: &[u8], index: usize) -> Result<H::Domain> {
    H::Domain::try_from_bytes(data_at_node(data, index).expect("invalid node math"))
}

fn get_even_column<H: Hasher>(encodings: &[Vec<u8>], layers: usize, x: usize) -> Result<Column<H>> {
    debug_assert_eq!(encodings.len(), layers - 1);

    let rows = (1..layers - 1)
        .step_by(2)
        .map(|layer| H::Domain::try_from_bytes(get_node_at_layer(encodings, x, layer)?))
        .collect::<Result<_>>()?;

    Ok(Column::new_even(x, rows))
}

fn get_odd_column<H: Hasher>(encodings: &[Vec<u8>], layers: usize, x: usize) -> Result<Column<H>> {
    debug_assert_eq!(encodings.len(), layers - 1);

    let rows = (0..layers)
        .step_by(2)
        .map(|layer| H::Domain::try_from_bytes(get_node_at_layer(encodings, x, layer)?))
        .collect::<Result<_>>()?;

    Ok(Column::new_odd(x, rows))
}

fn get_full_column<H: Hasher>(
    encodings: &[Vec<u8>],
    graph: &ZigZagBucketGraph<H>,
    layers: usize,
    x: usize,
) -> Result<Column<H>> {
    debug_assert_eq!(encodings.len(), layers - 1);

    let inv_index = graph.inv_index(x);

    let rows = (0..layers - 1)
        .map(|i| {
            let x = if (i + 1) % 2 == 0 { inv_index } else { x };

            H::Domain::try_from_bytes(get_node_at_layer(&encodings, x, i)?)
        })
        .collect::<Result<_>>()?;

    Ok(Column::new_all(x, rows))
}

fn get_node_at_layer(encodings: &[Vec<u8>], node: usize, layer: usize) -> Result<&[u8]> {
    data_at_node(&encodings[layer], node)
}

impl<'a, H: 'static + Hasher> ZigZagDrgPoRep<'a, H> {
    /// Transform a layer's public parameters, returning new public parameters corresponding to the next layer.
    /// Warning: This method will likely need to be extended for other implementations
    /// but because it is not clear what parameters they will need, only the ones needed
    /// for zizag are currently present (same applies to [invert_transform]).
    fn transform(graph: &ZigZagBucketGraph<H>) -> ZigZagBucketGraph<H> {
        graph.zigzag()
    }

    /// Transform a layer's public parameters, returning new public parameters corresponding to the previous layer.
    fn invert_transform(graph: &ZigZagBucketGraph<H>) -> ZigZagBucketGraph<H> {
        graph.zigzag()
    }

    #[allow(clippy::too_many_arguments)]
    fn prove_layers(
        graph_0: &ZigZagBucketGraph<H>,
        pub_inputs: &PublicInputs<<H as Hasher>::Domain>,
        p_aux: &PersistentAux<H::Domain>,
        t_aux: &TemporaryAux<H>,
        layer_challenges: &LayerChallenges,
        layers: usize,
        _total_layers: usize,
        partition_count: usize,
    ) -> Result<Vec<Proof<H>>> {
        assert!(layers > 0);
        assert_eq!(t_aux.encodings.len(), layers - 1);

        let graph_size = graph_0.size();
        assert_eq!(t_aux.es.len(), graph_size);
        assert_eq!(t_aux.os.len(), graph_size);

        let graph_1 = Self::transform(&graph_0);
        let graph_2 = Self::transform(&graph_1);

        assert_eq!(graph_0.layer(), 0);
        assert_eq!(graph_1.layer(), 1);
        assert_eq!(graph_2.layer(), 2);

        let get_drg_parents_columns = |x: usize| -> Result<Vec<Column<H>>> {
            let base_degree = graph_0.base_graph().degree();

            let mut columns = Vec::with_capacity(base_degree);

            let mut parents = vec![0; base_degree];
            graph_0.base_parents(x, &mut parents);

            for parent in &parents {
                columns.push(get_full_column(&t_aux.encodings, graph_0, layers, *parent)?);
            }

            debug_assert!(columns.len() == base_degree);

            Ok(columns)
        };

        let get_exp_parents_even_columns = |x: usize| -> Result<Vec<Column<H>>> {
            let exp_degree = graph_1.expansion_degree();

            let mut columns = Vec::with_capacity(exp_degree);

            let mut parents = vec![0; exp_degree];
            graph_1.expanded_parents(x, |p| {
                parents.copy_from_slice(p);
            });

            for parent in &parents {
                columns.push(get_even_column(&t_aux.encodings, layers, *parent as usize)?);
            }
            debug_assert!(columns.len() == exp_degree);

            Ok(columns)
        };

        let get_exp_parents_odd_columns = |x: usize| -> Result<Vec<Column<H>>> {
            let exp_degree = graph_2.expansion_degree();

            let mut columns = Vec::with_capacity(exp_degree);

            let mut parents = vec![0; exp_degree];
            graph_2.expanded_parents(x, |p| {
                parents.copy_from_slice(p);
            });

            for parent in &parents {
                columns.push(get_odd_column(&t_aux.encodings, layers, *parent as usize)?);
            }
            debug_assert!(columns.len() == exp_degree);

            Ok(columns)
        };

        (0..partition_count)
            .into_par_iter()
            .map(|k| {
                trace!("proving partition {}/{}", k + 1, partition_count);

                // Derive the set of challenges we are proving over.
                let challenges = pub_inputs.challenges(layer_challenges, graph_size, Some(k));

                let mut comm_d_proofs = Vec::with_capacity(challenges.len());
                let mut comm_r_last_proofs = Vec::with_capacity(challenges.len());

                let mut replica_column_proofs = Vec::with_capacity(challenges.len());

                let mut encoding_proof_1 = Vec::with_capacity(challenges.len());
                let mut encoding_proofs = Vec::with_capacity(challenges.len());

                // ZigZag commitment specifics
                for challenge in challenges {
                    trace!(" challenge {}", challenge);
                    debug_assert!(challenge < graph_0.size());

                    let inv_challenge = graph_0.inv_index(challenge);

                    // Initial data layer openings (D_X in Comm_D)
                    let comm_d_proof =
                        MerkleProof::new_from_proof(&t_aux.tree_d.gen_proof(challenge));

                    // ZigZag replica column openings
                    let rpc = {
                        // All labels in C_X
                        trace!("  c_x");
                        let c_x = {
                            let column =
                                get_full_column(&t_aux.encodings, &graph_0, layers, challenge)?;

                            let inclusion_proof = MerkleProof::new_from_proof(
                                &t_aux.tree_c.gen_proof(column.index()),
                            );
                            ColumnProof::<H>::all_from_column(column, inclusion_proof)
                        };

                        // Only odd-layer labels in the renumbered column C_\bar{X}
                        // WARNING: pulls the full column, as this is a bug in the spec atm.
                        trace!("  c_inv_x");
                        let c_inv_x = {
                            let column =
                                get_full_column(&t_aux.encodings, &graph_0, layers, inv_challenge)?;
                            let inclusion_proof = MerkleProof::new_from_proof(
                                &t_aux.tree_c.gen_proof(column.index()),
                            );
                            ColumnProof::<H>::all_from_column(column, inclusion_proof)
                        };

                        // All labels in the DRG parents.
                        trace!("  drg_parents");
                        let drg_parents = get_drg_parents_columns(challenge)?
                            .into_iter()
                            .map(|column| {
                                let inclusion_proof = MerkleProof::new_from_proof(
                                    &t_aux.tree_c.gen_proof(column.index()),
                                );
                                ColumnProof::<H>::all_from_column(column, inclusion_proof)
                            })
                            .collect::<Vec<_>>();

                        // Odd layer labels for the expander parents
                        trace!("  exp_parents_odd");
                        let exp_parents_odd = get_exp_parents_odd_columns(challenge)?
                            .into_iter()
                            .map(|column| {
                                let index = column.index();
                                let inclusion_proof =
                                    MerkleProof::new_from_proof(&t_aux.tree_c.gen_proof(index));
                                ColumnProof::<H>::odd_from_column(
                                    column,
                                    inclusion_proof,
                                    &t_aux.es[index],
                                )
                            })
                            .collect::<Vec<_>>();

                        // Even layer labels for the expander parents
                        trace!("  exp_parents_even");
                        let exp_parents_even = get_exp_parents_even_columns(inv_challenge)?
                            .into_iter()
                            .map(|column| {
                                let index = graph_1.inv_index(column.index());
                                let inclusion_proof =
                                    MerkleProof::new_from_proof(&t_aux.tree_c.gen_proof(index));
                                ColumnProof::<H>::even_from_column(
                                    column,
                                    inclusion_proof,
                                    &t_aux.os[index],
                                )
                            })
                            .collect::<Vec<_>>();

                        ReplicaColumnProof {
                            c_x,
                            c_inv_x,
                            drg_parents,
                            exp_parents_even,
                            exp_parents_odd,
                        }
                    };

                    // Final replica layer openings
                    trace!("final replica layer openings");
                    {
                        // All challenged Labels e_\bar{X}^(L)
                        trace!("  inclusion proof");
                        let inclusion_proof = MerkleProof::new_from_proof(
                            &t_aux.tree_r_last.gen_proof(inv_challenge),
                        );

                        // Even challenged parents (any kind)
                        trace!(" even parents");
                        let mut parents = vec![0; graph_1.degree()];
                        graph_1.parents(inv_challenge, &mut parents);

                        let even_parents_proof = parents
                            .into_iter()
                            .map(|parent| {
                                MerkleProof::new_from_proof(&t_aux.tree_r_last.gen_proof(parent))
                            })
                            .collect::<Vec<_>>();

                        comm_r_last_proofs.push((inclusion_proof, even_parents_proof));
                    }

                    // Encoding Proof layer 1
                    trace!("  encoding proof layer 1");
                    {
                        let encoded_node = rpc.c_x.get_verified_node_at_layer(1);

                        // TODO: pull this out of the inclusion proof
                        let decoded_node = comm_d_proof.verified_leaf();

                        let mut parents = vec![0; graph_0.degree()];
                        graph_0.parents(challenge, &mut parents);

                        let parents_data = parents
                            .into_iter()
                            .map(|parent| {
                                data_at_node(&t_aux.encodings[0], parent).map(|v| v.to_vec())
                            })
                            .collect::<Result<_>>()?;

                        encoding_proof_1.push(EncodingProof::<H>::new(
                            encoded_node,
                            decoded_node,
                            parents_data,
                        ));
                    }

                    // Encoding Proof Layer 2..l-1
                    {
                        let mut proofs = Vec::with_capacity(layers - 2);

                        for layer in 2..layers {
                            trace!("  encoding proof layer {}", layer);
                            let (graph, challenge, encoded_node, decoded_node) = if layer % 2 == 0 {
                                (
                                    &graph_1,
                                    inv_challenge,
                                    rpc.c_x.get_verified_node_at_layer(layer),
                                    rpc.c_inv_x.get_verified_node_at_layer(layer - 1),
                                )
                            } else {
                                (
                                    &graph_2,
                                    challenge,
                                    rpc.c_x.get_verified_node_at_layer(layer),
                                    rpc.c_inv_x.get_verified_node_at_layer(layer - 1),
                                )
                            };

                            let encoded_data = &t_aux.encodings[layer - 1];

                            let mut parents = vec![0; graph.degree()];
                            graph.parents(challenge, &mut parents);

                            let parents_data = parents
                                .into_iter()
                                .map(|parent| {
                                    data_at_node(&encoded_data, parent).map(|v| v.to_vec())
                                })
                                .collect::<Result<_>>()?;

                            let proof = EncodingProof::<H>::new(
                                encoded_node.clone(),
                                decoded_node.clone(),
                                parents_data,
                            );

                            debug_assert!(
                                proof.verify(&pub_inputs.replica_id, &encoded_node, &decoded_node),
                                "Invalid encoding proof generated"
                            );

                            proofs.push(proof);
                        }

                        encoding_proofs.push(proofs);
                    }

                    comm_d_proofs.push(comm_d_proof);
                    replica_column_proofs.push(rpc);
                }

                Ok(Proof {
                    comm_d_proofs,
                    replica_column_proofs,
                    comm_r_last_proofs,
                    encoding_proof_1,
                    encoding_proofs,
                })
            })
            .collect()
    }

    fn extract_and_invert_transform_layers(
        graph: &ZigZagBucketGraph<H>,
        layer_challenges: &LayerChallenges,
        replica_id: &<H as Hasher>::Domain,
        data: &mut [u8],
    ) -> Result<()> {
        trace!("extract_and_invert_transform_layers");

        let layers = layer_challenges.layers();
        assert!(layers > 0);

        (0..layers).fold(graph.clone(), |current_graph, _layer| {
            let inverted = Self::invert_transform(&current_graph);
            let pp = drgporep::PublicParams::new(
                inverted.clone(),
                true,
                layer_challenges.challenges_count(),
            );
            let mut res = DrgPoRep::extract_all(&pp, replica_id, data)
                .expect("failed to extract data from PoRep");

            for (i, r) in res.iter_mut().enumerate() {
                data[i] = *r;
            }
            inverted
        });

        Ok(())
    }

    fn transform_and_replicate_layers(
        graph: &ZigZagBucketGraph<H>,
        layer_challenges: &LayerChallenges,
        replica_id: &<H as Hasher>::Domain,
        data: &mut [u8],
        data_tree: Option<Tree<H>>,
    ) -> Result<TransformedLayers<H>> {
        trace!("transform_and_replicate_layers");
        let nodes_count = graph.size();

        assert_eq!(data.len(), nodes_count * NODE_SIZE);

        // TODO:
        // The implementation below is a memory hog, and very naive in terms of performance.
        // It also hardcodes the hash function.
        // This is done to get an initial version implemented and make sure it is correct.
        // After that we can improve on that.

        let layers = layer_challenges.layers();
        assert!(layers > 0);

        let build_tree = |tree_data: &[u8]| {
            trace!("building tree {}", tree_data.len());

            let leafs = tree_data.len() / NODE_SIZE;
            assert_eq!(tree_data.len() % NODE_SIZE, 0);
            let pow = next_pow2(leafs);
            let mut leaves_store = MerkleStore::new(pow);
            populate_leaves::<_, <H as Hasher>::Function, _, std::iter::Map<_, _>>(
                &mut leaves_store,
                (0..leafs).map(|i| get_node::<H>(tree_data, i).unwrap()),
            );

            graph.merkle_tree_from_leaves(leaves_store, leafs)
        };

        // 1. Build the MerkleTree over the original data
        trace!("build merkle tree for the original data");
        let tree_d = match data_tree {
            Some(t) => t,
            None => build_tree(&data)?,
        };

        // 2. Encode all layers
        trace!("encode layers");
        let mut encodings: Vec<Vec<u8>> = Vec::with_capacity(layers - 1);
        let mut current_graph = graph.clone();
        let mut to_encode = data.to_vec();

        for layer in 0..layers {
            trace!("encoding (layer: {})", layer);
            vde::encode(&current_graph, replica_id, &mut to_encode)?;
            current_graph = Self::transform(&current_graph);

            assert_eq!(to_encode.len(), NODE_SIZE * nodes_count);

            if layer != layers - 1 {
                let p = to_encode.clone();
                encodings.push(p);
            }
        }

        assert_eq!(encodings.len(), layers - 1);

        let r_last = to_encode;

        // store the last layer in the original data
        data[..NODE_SIZE * nodes_count].copy_from_slice(&r_last);

        // 3. Construct Column Commitments
        let odd_columns = (0..nodes_count)
            .into_par_iter()
            .map(|x| get_odd_column::<H>(&encodings, layers, x));

        let even_columns = (0..nodes_count)
            .into_par_iter()
            .map(|x| get_even_column::<H>(&encodings, layers, graph.inv_index(x)));

        // O_i = H( e_i^(1) || .. )
        let os = odd_columns
            .map(|c| c.map(|c| c.hash()))
            .collect::<Result<Vec<_>>>()?;

        // E_i = H( e_\bar{i}^(2) || .. )
        let es = even_columns
            .map(|c| c.map(|c| c.hash()))
            .collect::<Result<Vec<_>>>()?;

        // C_i = H(O_i || E_i)
        let cs = os
            .par_iter()
            .zip(es.par_iter())
            .flat_map(|(o_i, e_i)| hash2(&o_i[..], &e_i[..]))
            .collect::<Vec<_>>();

        // Build the tree for CommC
        let tree_c = build_tree(&cs)?;

        // sanity check
        debug_assert_eq!(tree_c.read_at(0).as_ref(), &cs[..NODE_SIZE]);
        debug_assert_eq!(tree_c.read_at(1).as_ref(), &cs[NODE_SIZE..NODE_SIZE * 2]);

        // 4. Construct final replica commitment
        let tree_r_last = build_tree(&r_last)?;

        // comm_r = H(comm_c || comm_r_last)
        let comm_r = {
            let mut bytes = tree_c.root().as_ref().to_vec();
            bytes.extend_from_slice(tree_r_last.root().as_ref());
            <H as Hasher>::Function::hash(&bytes)
        };

        Ok((
            Tau {
                comm_d: tree_d.root(),
                comm_r,
            },
            PersistentAux {
                comm_c: tree_c.root(),
                comm_r_last: tree_r_last.root(),
            },
            TemporaryAux {
                encodings,
                es,
                os,
                tree_c,
                tree_d,
                tree_r_last,
            },
        ))
    }
}

impl<'a, 'c, H: 'static + Hasher> ProofScheme<'a> for ZigZagDrgPoRep<'c, H> {
    type PublicParams = PublicParams<H, ZigZagBucketGraph<H>>;
    type SetupParams = SetupParams;
    type PublicInputs = PublicInputs<<H as Hasher>::Domain>;
    type PrivateInputs = PrivateInputs<H>;
    type Proof = Proof<H>;
    type Requirements = ChallengeRequirements;

    fn setup(sp: &Self::SetupParams) -> Result<Self::PublicParams> {
        let graph = ZigZagBucketGraph::<H>::new_zigzag(
            sp.drg.nodes,
            sp.drg.degree,
            sp.drg.expansion_degree,
            0,
            sp.drg.seed,
        );

        Ok(PublicParams::new(graph, sp.layer_challenges.clone()))
    }

    fn prove<'b>(
        pub_params: &'b Self::PublicParams,
        pub_inputs: &'b Self::PublicInputs,
        priv_inputs: &'b Self::PrivateInputs,
    ) -> Result<Self::Proof> {
        let proofs = Self::prove_all_partitions(pub_params, pub_inputs, priv_inputs, 1)?;
        let k = match pub_inputs.k {
            None => 0,
            Some(k) => k,
        };

        Ok(proofs[k].to_owned())
    }

    fn prove_all_partitions<'b>(
        pub_params: &'b Self::PublicParams,
        pub_inputs: &'b Self::PublicInputs,
        priv_inputs: &'b Self::PrivateInputs,
        partition_count: usize,
    ) -> Result<Vec<Self::Proof>> {
        trace!("prove_all_partitions");
        assert!(partition_count > 0);

        Self::prove_layers(
            &pub_params.graph,
            pub_inputs,
            &priv_inputs.p_aux,
            &priv_inputs.t_aux,
            &pub_params.layer_challenges,
            pub_params.layer_challenges.layers(),
            pub_params.layer_challenges.layers(),
            partition_count,
        )
    }

    fn verify_all_partitions(
        pub_params: &Self::PublicParams,
        pub_inputs: &Self::PublicInputs,
        partition_proofs: &[Self::Proof],
    ) -> Result<bool> {
        trace!("verify_all_partitions");

        // generate graphs
        let graph_0 = &pub_params.graph;
        let graph_1 = Self::transform(graph_0);
        let graph_2 = Self::transform(&graph_1);

        assert_eq!(graph_0.layer(), 0);
        assert_eq!(graph_1.layer(), 1);
        assert_eq!(graph_2.layer(), 2);

        let replica_id = &pub_inputs.replica_id;
        let layers = pub_params.layer_challenges.layers();

        let valid = partition_proofs
            .into_par_iter()
            .enumerate()
            .all(|(k, proof)| {
                trace!(
                    "verifying partition proof {}/{}",
                    k + 1,
                    partition_proofs.len()
                );

                // TODO:
                // 1. grab all comm_r_last and ensure they are the same (from inclusion proofs)
                // 2. grab all comm_c and ensure they are the same (from inclusion proofs)
                // 3. check that H(comm_c || comm_r_last) == comm_r

                let challenges =
                    pub_inputs.challenges(&pub_params.layer_challenges, graph_0.size(), Some(k));
                for i in 0..challenges.len() {
                    trace!("verify challenge {}/{}", i, challenges.len());
                    // Validate for this challenge
                    let challenge = challenges[i] % graph_0.size();

                    // Verify initial data layer
                    trace!("verify initial data layer");
                    check!(proof.comm_d_proofs[i].proves_challenge(challenge));

                    check_eq!(proof.comm_d_proofs[i].root(), &pub_inputs.tau.comm_d);

                    // Verify replica column openings
                    trace!("verify replica column openings");
                    {
                        let rco = &proof.replica_column_proofs[i];

                        trace!("  verify c_x");
                        check!(rco.c_x.verify());

                        trace!("  verify c_inv_x");
                        check!(rco.c_inv_x.verify());

                        trace!("  verify drg_parents");
                        for proof in &rco.drg_parents {
                            check!(proof.verify());
                        }

                        trace!("  verify exp_parents_even");
                        for proof in &rco.exp_parents_even {
                            check!(proof.verify());
                        }

                        trace!("  verify exp_parents_odd");
                        for proof in &rco.exp_parents_odd {
                            check!(proof.verify());
                        }
                    }

                    // Verify final replica layer openings
                    trace!("verify final replica layer openings");
                    {
                        let inv_challenge = graph_0.inv_index(challenge);

                        check!(proof.comm_r_last_proofs[i]
                            .0
                            .proves_challenge(inv_challenge));

                        let mut parents = vec![0; graph_1.degree()];
                        graph_1.parents(inv_challenge, &mut parents);

                        check_eq!(parents.len(), proof.comm_r_last_proofs[i].1.len());

                        for (p, parent) in proof.comm_r_last_proofs[i]
                            .1
                            .iter()
                            .zip(parents.into_iter())
                        {
                            check!(p.proves_challenge(parent));
                        }
                    }

                    // Verify Encoding Layer 1
                    trace!("verify encoding (layer: 1)");
                    let rpc = &proof.replica_column_proofs[i];
                    let comm_d = &proof.comm_d_proofs[i];

                    check!(proof.encoding_proof_1[i].verify(
                        replica_id,
                        rpc.c_x.get_node_at_layer(1),
                        comm_d.leaf()
                    ));

                    // Verify Encoding Layer 2..layers - 1
                    {
                        assert_eq!(proof.encoding_proofs[i].len(), layers - 2);
                        for (j, encoding_proof) in proof.encoding_proofs[i].iter().enumerate() {
                            let layer = j + 2;
                            trace!("verify encoding (layer: {})", layer);;
                            let (encoded_node, decoded_node) = if layer % 2 == 0 {
                                (
                                    rpc.c_x.get_node_at_layer(layer),
                                    rpc.c_inv_x.get_node_at_layer(layer - 1),
                                )
                            } else {
                                (
                                    rpc.c_x.get_node_at_layer(layer),
                                    rpc.c_inv_x.get_node_at_layer(layer - 1),
                                )
                            };
                            check!(encoding_proof.verify(replica_id, encoded_node, decoded_node));
                        }
                    }
                }

                true
            });

        Ok(valid)
    }

    fn with_partition(pub_in: Self::PublicInputs, k: Option<usize>) -> Self::PublicInputs {
        self::PublicInputs {
            replica_id: pub_in.replica_id,
            seed: None,
            tau: pub_in.tau,
            k,
        }
    }

    fn satisfies_requirements(
        public_params: &PublicParams<H, ZigZagBucketGraph<H>>,
        requirements: &ChallengeRequirements,
        partitions: usize,
    ) -> bool {
        let partition_challenges = public_params.layer_challenges.challenges_count();

        partition_challenges * partitions >= requirements.minimum_challenges
    }
}

impl<'a, 'c, H: 'static + Hasher> PoRep<'a, H> for ZigZagDrgPoRep<'a, H> {
    type Tau = Tau<<H as Hasher>::Domain>;
    type ProverAux = (PersistentAux<H::Domain>, TemporaryAux<H>);

    fn replicate(
        pp: &'a PublicParams<H, ZigZagBucketGraph<H>>,
        replica_id: &<H as Hasher>::Domain,
        data: &mut [u8],
        data_tree: Option<Tree<H>>,
    ) -> Result<(Self::Tau, Self::ProverAux)> {
        let (tau, p_aux, t_aux) = Self::transform_and_replicate_layers(
            &pp.graph,
            &pp.layer_challenges,
            replica_id,
            data,
            data_tree,
        )?;

        Ok((tau, (p_aux, t_aux)))
    }

    fn extract_all<'b>(
        pp: &'b PublicParams<H, ZigZagBucketGraph<H>>,
        replica_id: &'b <H as Hasher>::Domain,
        data: &'b [u8],
    ) -> Result<Vec<u8>> {
        let mut data = data.to_vec();

        Self::extract_and_invert_transform_layers(
            &pp.graph,
            &pp.layer_challenges,
            replica_id,
            &mut data,
        )?;

        Ok(data)
    }

    fn extract(
        _pp: &PublicParams<H, ZigZagBucketGraph<H>>,
        _replica_id: &<H as Hasher>::Domain,
        _data: &[u8],
        _node: usize,
    ) -> Result<Vec<u8>> {
        unimplemented!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use paired::bls12_381::Bls12;
    use rand::{Rng, SeedableRng, XorShiftRng};

    use crate::drgporep;
    use crate::drgraph::{new_seed, BASE_DEGREE};
    use crate::fr32::fr_into_bytes;
    use crate::hasher::blake2s::Blake2sDomain;
    use crate::hasher::{Blake2sHasher, PedersenHasher, Sha256Hasher};
    use crate::porep::PoRep;
    use crate::proof::ProofScheme;
    use crate::zigzag::EXP_DEGREE;

    const DEFAULT_ZIGZAG_LAYERS: usize = 10;

    #[test]
    fn test_calculate_fixed_challenges() {
        let layer_challenges = LayerChallenges::new_fixed(10, 333);
        let expected = 333;

        let calculated_count = layer_challenges.challenges_count();
        assert_eq!(expected as usize, calculated_count);
    }

    #[test]
    fn extract_all_pedersen() {
        test_extract_all::<PedersenHasher>();
    }

    #[test]
    fn extract_all_sha256() {
        test_extract_all::<Sha256Hasher>();
    }

    #[test]
    fn extract_all_blake2s() {
        test_extract_all::<Blake2sHasher>();
    }

    fn test_extract_all<H: 'static + Hasher>() {
        let rng = &mut XorShiftRng::from_seed([0x3dbe6259, 0x8d313d76, 0x3237db17, 0xe5bc0654]);
        let replica_id: H::Domain = rng.gen();
        let data = vec![2u8; 32 * 3];
        let challenges = LayerChallenges::new_fixed(DEFAULT_ZIGZAG_LAYERS, 5);

        // create a copy, so we can compare roundtrips
        let mut data_copy = data.clone();

        let sp = SetupParams {
            drg: drgporep::DrgParams {
                nodes: data.len() / 32,
                degree: BASE_DEGREE,
                expansion_degree: EXP_DEGREE,
                seed: new_seed(),
            },
            layer_challenges: challenges.clone(),
        };

        let mut pp = ZigZagDrgPoRep::<H>::setup(&sp).expect("setup failed");
        // Get the graph for the last layer.
        // In reality, this is a no-op with an even number of layers.
        for _ in 0..pp.layer_challenges.layers() {
            pp.graph = pp.graph.zigzag();
        }

        ZigZagDrgPoRep::<H>::replicate(&pp, &replica_id, data_copy.as_mut_slice(), None)
            .expect("replication failed");

        let transformed_params = PublicParams::new(pp.graph, challenges.clone());

        assert_ne!(data, data_copy);

        let decoded_data = ZigZagDrgPoRep::<H>::extract_all(
            &transformed_params,
            &replica_id,
            data_copy.as_mut_slice(),
        )
        .expect("failed to extract data");

        assert_eq!(data, decoded_data);
    }

    fn prove_verify_fixed(n: usize) {
        let challenges = LayerChallenges::new_fixed(DEFAULT_ZIGZAG_LAYERS, 5);

        test_prove_verify::<PedersenHasher>(n, challenges.clone());
        test_prove_verify::<Sha256Hasher>(n, challenges.clone());
        test_prove_verify::<Blake2sHasher>(n, challenges.clone());
    }

    fn test_prove_verify<H: 'static + Hasher>(n: usize, challenges: LayerChallenges) {
        // This will be called multiple times, only the first one succeeds, and that is ok.
        femme::pretty::Logger::new()
            .start(log::LevelFilter::Trace)
            .ok();

        let rng = &mut XorShiftRng::from_seed([0x3dbe6259, 0x8d313d76, 0x3237db17, 0xe5bc0654]);

        let degree = BASE_DEGREE;
        let expansion_degree = EXP_DEGREE;
        let replica_id: H::Domain = rng.gen();
        let data: Vec<u8> = (0..n)
            .flat_map(|_| fr_into_bytes::<Bls12>(&rng.gen()))
            .collect();
        // create a copy, so we can compare roundtrips
        let mut data_copy = data.clone();
        let partitions = 2;

        let sp = SetupParams {
            drg: drgporep::DrgParams {
                nodes: n,
                degree,
                expansion_degree,
                seed: new_seed(),
            },
            layer_challenges: challenges.clone(),
        };

        let pp = ZigZagDrgPoRep::<H>::setup(&sp).expect("setup failed");
        let (tau, (p_aux, t_aux)) =
            ZigZagDrgPoRep::<H>::replicate(&pp, &replica_id, data_copy.as_mut_slice(), None)
                .expect("replication failed");
        assert_ne!(data, data_copy);

        let pub_inputs = PublicInputs::<H::Domain> {
            replica_id,
            seed: None,
            tau,
            k: None,
        };

        let priv_inputs = PrivateInputs { p_aux, t_aux };

        let all_partition_proofs =
            &ZigZagDrgPoRep::<H>::prove_all_partitions(&pp, &pub_inputs, &priv_inputs, partitions)
                .expect("failed to generate partition proofs");

        let proofs_are_valid =
            ZigZagDrgPoRep::<H>::verify_all_partitions(&pp, &pub_inputs, all_partition_proofs)
                .expect("failed to verify partition proofs");

        assert!(proofs_are_valid);
    }

    table_tests! {
        prove_verify_fixed{
           prove_verify_fixed_32_4(4);
        }
    }

    #[test]
    // We are seeing a bug, in which setup never terminates for some sector sizes.
    // This test is to debug that and should remain as a regression teset.
    fn setup_terminates() {
        let degree = BASE_DEGREE;
        let expansion_degree = EXP_DEGREE;
        let nodes = 1024 * 1024 * 32 * 8; // This corresponds to 8GiB sectors (32-byte nodes)
        let layer_challenges = LayerChallenges::new_fixed(10, 333);
        let sp = SetupParams {
            drg: drgporep::DrgParams {
                nodes,
                degree,
                expansion_degree,
                seed: new_seed(),
            },
            layer_challenges: layer_challenges.clone(),
        };

        // When this fails, the call to setup should panic, but seems to actually hang (i.e. neither return nor panic) for some reason.
        // When working as designed, the call to setup returns without error.
        let _pp = ZigZagDrgPoRep::<PedersenHasher>::setup(&sp).expect("setup failed");
    }

    #[test]
    fn test_odd_column() {
        let encodings = vec![
            vec![1; NODE_SIZE],
            vec![2; NODE_SIZE],
            vec![3; NODE_SIZE],
            vec![4; NODE_SIZE],
            vec![5; NODE_SIZE],
        ];

        assert_eq!(
            get_odd_column::<Blake2sHasher>(&encodings, 6, 0).unwrap(),
            Column::new_odd(
                0,
                vec![
                    Blake2sDomain::try_from_bytes(&vec![1; NODE_SIZE]).unwrap(),
                    Blake2sDomain::try_from_bytes(&vec![3; NODE_SIZE]).unwrap(),
                    Blake2sDomain::try_from_bytes(&vec![5; NODE_SIZE]).unwrap(),
                ]
            )
        );
    }

    #[test]
    fn test_even_column() {
        let encodings = vec![
            vec![1; NODE_SIZE],
            vec![2; NODE_SIZE],
            vec![3; NODE_SIZE],
            vec![4; NODE_SIZE],
            vec![5; NODE_SIZE],
        ];

        assert_eq!(
            get_even_column::<Blake2sHasher>(&encodings, 6, 0).unwrap(),
            Column::new_even(
                0,
                vec![
                    Blake2sDomain::try_from_bytes(&vec![2; NODE_SIZE]).unwrap(),
                    Blake2sDomain::try_from_bytes(&vec![4; NODE_SIZE]).unwrap(),
                ]
            ),
        );
    }

    #[test]
    fn test_full_column() {
        use itertools::Itertools;
        let nodes: usize = 8;

        let make_nodes = |x| {
            let mut res = Vec::new();
            for i in 0..nodes {
                res.extend_from_slice(&vec![x as u8; NODE_SIZE / 2]);
                res.extend_from_slice(&vec![i as u8; NODE_SIZE / 2]);
            }
            res
        };

        let encodings: Vec<Vec<u8>> = vec![
            make_nodes(1),
            make_nodes(2),
            make_nodes(3),
            make_nodes(4),
            make_nodes(5),
        ];

        let graph = ZigZagBucketGraph::<Blake2sHasher>::new_zigzag(
            nodes,
            BASE_DEGREE,
            EXP_DEGREE,
            0,
            new_seed(),
        );

        for node in 0..nodes {
            let even =
                get_even_column::<Blake2sHasher>(&encodings, 6, graph.inv_index(node)).unwrap();
            let odd = get_odd_column::<Blake2sHasher>(&encodings, 6, node).unwrap();
            let all = get_full_column::<Blake2sHasher>(&encodings, &graph, 6, node).unwrap();
            assert_eq!(all.index(), node);

            assert_eq!(
                odd.rows()
                    .iter()
                    .cloned()
                    .interleave(even.rows().iter().cloned())
                    .collect::<Vec<_>>(),
                all.rows().clone(),
            );

            let col_hash = all.hash();
            let e_hash = even.hash();
            let o_hash = odd.hash();
            let combined_hash = hash2(&o_hash, &e_hash);

            assert_eq!(col_hash, combined_hash);
        }
    }
}