use super::node::*;
use super::vars::*;
use super::GraphError;
use crate::circuit::lookup::Config as LookupConfig;
use crate::circuit::lookup::Op as LookupOp;
use crate::circuit::lookup::Table as LookupTable;
use crate::circuit::polynomial::Config as PolyConfig;
use crate::circuit::polynomial::InputType as PolyInputType;
use crate::circuit::polynomial::Node as PolyNode;
use crate::circuit::polynomial::Op as PolyOp;

// use crate::circuit::polynomial::InputType as PolyInputType;

use crate::circuit::range::*;
use crate::commands::{Cli, Commands};
use crate::tensor::TensorType;
use crate::tensor::{Tensor, ValTensor, VarTensor};
//use clap::Parser;
use anyhow::{Context, Error as AnyError};
use halo2_proofs::{
    arithmetic::FieldExt,
    circuit::{Layouter, Value},
    plonk::ConstraintSystem,
};
use itertools::Itertools;
use log::{debug, info, trace};
use std::cell::RefCell;
use std::cmp::max;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::path::Path;
use std::rc::Rc;
use tabled::Table;
use tract_onnx;
use tract_onnx::prelude::{Framework, Graph, InferenceFact, Node as OnnxNode, OutletId};
use tract_onnx::tract_hir::internal::InferenceOp;
/// Mode we're using the model in.
#[derive(Clone, Debug)]
pub enum Mode {
    /// Initialize the model and display the operations table / graph
    Table,
    /// Initialize the model and generate a mock proof
    Mock,
    /// Initialize the model and generate a proof
    Prove,
    /// Initialize the model, generate a proof, and verify
    FullProve,
    /// Initialize the model and verify an already generated proof
    Verify,
}

/// A circuit configuration for the entirety of a model loaded from an Onnx file.
#[derive(Clone, Debug)]
pub struct ModelConfig<F: FieldExt + TensorType> {
    configs: BTreeMap<usize, NodeConfig<F>>,
    /// The model struct
    pub model: Model,
    /// (optional) range checked outputs of the model graph
    pub public_outputs: Vec<RangeCheckConfig<F>>,
    /// A wrapper for holding all columns that will be assigned to by the model
    pub vars: ModelVars<F>,
}

/// A struct for loading from an Onnx file and converting a computational graph to a circuit.
#[derive(Clone, Debug)]
pub struct Model {
    /// The raw tract [Graph] data structure.
    pub model: Graph<InferenceFact, Box<dyn InferenceOp>>,
    /// Graph of nodes we are loading from Onnx.
    pub nodes: NodeGraph, // Wrapped nodes with additional methods and data (e.g. inferred shape, quantization)
    /// bits used in lookup tables
    pub bits: usize,
    /// Log rows available in circuit.
    pub logrows: u32,
    /// Maximum number of permitted rotations.
    pub max_rotations: usize,
    /// Exponent used in the fixed point representation.
    pub scale: i32,
    /// The divergence from the expected output (if using public outputs) we can tolerate. This is in absolute value across each dimension.
    /// eg. for a tolerance of 1 and for a 2D output we could tolerate at most off by 1 errors for each of the 2 outputs.
    pub tolerance: usize,
    /// The [Mode] we're using the model in.
    pub mode: Mode,
    /// Defines which inputs to the model are public and private (params, inputs, outputs) using [VarVisibility].
    pub visibility: VarVisibility,
}

impl Model {
    /// Creates an `Model` from a specified path to an Onnx file.
    /// # Arguments
    ///
    /// * `path` - A path to an Onnx file.
    /// * `scale` - The denominator used for fixed point arithmetic (relevant for quantizing input data and model parameters).
    /// * `bits` - Number of bits to use.
    /// * `logrows` -  Log rows available in circuit.
    /// * `max_rotations` - Maximum number of permitted rotations.
    /// * `tolerance` - How much each quantized output is allowed to be off by
    /// * `mode` - The [Mode] we're using the model in.
    /// * `visibility` - Which inputs to the model are public and private (params, inputs, outputs) using [VarVisibility].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: impl AsRef<Path>,
        scale: i32,
        bits: usize,
        logrows: u32,
        max_rotations: usize,
        tolerance: usize,
        mode: Mode,
        visibility: VarVisibility,
    ) -> Result<Self, Box<dyn Error>> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(|_| GraphError::ModelLoad)?;
        info!("visibility: {}", visibility);

        let mut nodes = BTreeMap::<usize, Node>::new();
        for (i, n) in model.nodes.iter().enumerate() {
            let n = Node::new(n.clone(), &mut nodes, scale, i)?;
            nodes.insert(i, n);
        }
        let om = Model {
            model: model.clone(),
            scale,
            tolerance,
            nodes: Self::assign_execution_buckets(nodes)?,
            bits,
            logrows,
            max_rotations,
            mode,
            visibility,
        };

        debug!("{}", Table::new(om.nodes.flatten()).to_string());

        Ok(om)
    }

    /// Creates a `Model` from parsed CLI arguments
    pub fn from_ezkl_conf(args: Cli) -> Result<Self, Box<dyn Error>> {
        let visibility = VarVisibility::from_args(args.clone())?;
        match args.command {
            Commands::Table { model } | Commands::Mock { model, .. } => Model::new(
                model,
                args.scale,
                args.bits,
                args.logrows,
                args.max_rotations,
                args.tolerance,
                Mode::Table,
                visibility,
            ),
            Commands::CreateEVMVerifier { model, .. }
            | Commands::Prove { model, .. }
            | Commands::Verify { model, .. }
            | Commands::Aggregate { model, .. } => Model::new(
                model,
                args.scale,
                args.bits,
                args.logrows,
                args.max_rotations,
                args.tolerance,
                Mode::Table,
                visibility,
            ),
            _ => panic!(),
        }
    }

    /// Creates a `Model` based on CLI arguments
    pub fn from_arg() -> Result<Self, Box<dyn Error>> {
        let args = Cli::create();
        Self::from_ezkl_conf(args)
    }

    /// Configures an `Model`. Does so one execution `bucket` at a time. Each bucket holds either:
    /// a) independent lookup operations (i.e operations that don't feed into one another so can be processed in parallel).
    /// b) operations that can be fused together, i.e the output of one op might feed into another.
    /// # Arguments
    ///
    /// * `meta` - Halo2 ConstraintSystem.
    /// * `advices` - A `VarTensor` holding columns of advices. Must be sufficiently large to configure all the nodes loaded in `self.nodes`.
    pub fn configure<F: FieldExt + TensorType>(
        &self,
        meta: &mut ConstraintSystem<F>,
        vars: &mut ModelVars<F>,
    ) -> Result<ModelConfig<F>, Box<dyn Error>> {
        info!("configuring model");
        let mut results = BTreeMap::new();
        let mut tables = BTreeMap::new();

        for (bucket, bucket_nodes) in self.nodes.0.iter() {
            trace!("configuring bucket: {:?}", bucket);
            let non_op_nodes: BTreeMap<&usize, &Node> = bucket_nodes
                .iter()
                .filter(|(_, n)| n.opkind.is_const() || n.opkind.is_input())
                .collect();
            if !non_op_nodes.is_empty() {
                for (i, node) in non_op_nodes {
                    let config = self.conf_non_op_node(node)?;
                    results.insert(*i, config);
                }
            }

            let lookup_ops: BTreeMap<&usize, &Node> = bucket_nodes
                .iter()
                .filter(|(_, n)| n.opkind.is_lookup())
                .collect();

            if !lookup_ops.is_empty() {
                for (i, node) in lookup_ops {
                    let config = self.conf_table(node, meta, vars, &mut tables)?;
                    results.insert(*i, config);
                }
            }

            // preserves ordering
            let poly_ops: BTreeMap<&usize, &Node> = bucket_nodes
                .iter()
                .filter(|(_, n)| n.opkind.is_poly())
                .collect();
            // preserves ordering
            if !poly_ops.is_empty() {
                let config = self.conf_poly_ops(&poly_ops, meta, vars)?;
                results.insert(**poly_ops.keys().max().unwrap(), config);

                let mut display: String = "Poly nodes: ".to_string();
                for idx in poly_ops.keys().map(|k| **k).sorted() {
                    let node = &self.nodes.filter(idx);
                    display.push_str(&format!("| {} ({:?}) | ", idx, node.opkind));
                }
                trace!("{}", display);
            }
        }

        let mut public_outputs = vec![];
        if self.visibility.output.is_public() {
            public_outputs = self.range_check_outputs(meta, vars)
        };

        Ok(ModelConfig {
            configs: results,
            model: self.clone(),
            public_outputs,
            vars: vars.clone(),
        })
    }

    fn range_check_outputs<F: FieldExt + TensorType>(
        &self,
        meta: &mut ConstraintSystem<F>,
        vars: &mut ModelVars<F>,
    ) -> Vec<RangeCheckConfig<F>> {
        let mut configs = vec![];
        let output_nodes = self.model.outputs.clone();
        let output_shapes = output_nodes
            .iter()
            .map(|o| self.nodes.filter(o.node).out_dims)
            .collect_vec();

        info!("output_shapes {:?}", output_shapes);

        for s in &output_shapes {
            let input = vars.advices[0].reshape(s);
            let output = vars.advices[1].reshape(s);

            configs.push(RangeCheckConfig::configure(
                meta,
                &input,
                &output,
                self.tolerance,
            ));
        }
        configs
    }
    /// Configures non op related nodes (eg. representing an input or const value)
    pub fn conf_non_op_node<F: FieldExt + TensorType>(
        &self,
        node: &Node,
    ) -> Result<NodeConfig<F>, Box<dyn Error>> {
        match &node.opkind {
            OpKind::Const => {
                // Typically parameters for one or more layers.
                // Currently this is handled in the consuming node(s), but will be moved here.
                Ok(NodeConfig::Const)
            }
            OpKind::Input => {
                // This is the input to the model (e.g. the image).
                // Currently this is handled in the consuming node(s), but will be moved here.
                Ok(NodeConfig::Input)
            }
            OpKind::Unknown(_c) => {
                unimplemented!()
            }
            c => Err(Box::new(GraphError::WrongMethod(node.idx, c.clone()))),
        }
    }

    /// Configures a [BTreeMap] of operations that can be constrained using polynomials. These correspond to operations that are represented in
    /// the `circuit::polynomial` module. A single configuration is output, representing the amalgamation of these operations into
    /// a single Halo2 gate.
    /// # Arguments
    ///
    /// * `nodes` - A [BTreeMap] of (node index, [Node] pairs). The [Node] must represent a polynomial op.
    /// * `meta` - Halo2 ConstraintSystem.
    /// * `vars` - [ModelVars] for the model.
    fn conf_poly_ops<F: FieldExt + TensorType>(
        &self,
        nodes: &BTreeMap<&usize, &Node>,
        meta: &mut ConstraintSystem<F>,
        vars: &mut ModelVars<F>,
    ) -> Result<NodeConfig<F>, Box<dyn Error>> {
        let mut input_nodes: BTreeMap<(&usize, &PolyOp), Vec<Node>> = BTreeMap::new();

        for (i, e) in nodes.iter() {
            let key = (
                *i,
                match &e.opkind {
                    OpKind::Poly(f) => f,
                    _ => {
                        return Err(Box::new(GraphError::WrongMethod(e.idx, e.opkind.clone())));
                    }
                },
            );
            let value = e
                .inputs
                .iter()
                .map(|i| self.nodes.filter(i.node))
                .collect_vec();
            input_nodes.insert(key, value);
        }

        // This works because retain only keeps items for which the predicate returns true, and
        // insert only returns true if the item was not previously present in the set.
        // Since the vector is traversed in order, we end up keeping just the first occurrence of each item.
        let mut seen = HashSet::new();
        let mut advice_idx = 0;
        let mut fixed_idx = 0;
        // impose an execution order here
        let inputs_to_layer: Vec<(usize, VarTensor)> = input_nodes
            .iter()
            .flat_map(|x| {
                x.1.iter()
                    .filter(|i| !nodes.contains_key(&i.idx) && seen.insert(i.idx))
                    .map(|f| {
                        let s = f.out_dims.clone();
                        if f.opkind.is_const() && self.visibility.params.is_public() {
                            let vars = (f.idx, vars.fixed[fixed_idx].reshape(&s));
                            fixed_idx += 1;
                            vars
                        } else {
                            let vars = (f.idx, vars.advices[advice_idx].reshape(&s));
                            advice_idx += 1;
                            vars
                        }
                    })
                    .collect_vec()
            })
            .collect_vec();

        let output_shape = self.nodes.filter(**nodes.keys().max().unwrap()).out_dims;
        // output node
        let output = &vars.advices[advice_idx].reshape(&output_shape);

        let mut inter_counter = 0;
        let fused_nodes: Vec<PolyNode> = input_nodes
            .iter()
            .map(|(op, e)| {
                let order = e
                    .iter()
                    .map(|n| {
                        if !nodes.contains_key(&n.idx) {
                            PolyInputType::Input(
                                inputs_to_layer.iter().position(|r| r.0 == n.idx).unwrap(),
                            )
                        } else {
                            inter_counter += 1;
                            PolyInputType::Inter(inter_counter - 1)
                        }
                    })
                    .collect_vec();
                PolyNode {
                    op: op.1.clone(),
                    input_order: order,
                }
            })
            .collect_vec();

        let inputs = inputs_to_layer.iter();

        let config = NodeConfig::Poly(
            PolyConfig::configure(
                meta,
                &inputs.clone().map(|x| x.1.clone()).collect_vec(),
                output,
                &fused_nodes,
            ),
            inputs.map(|x| x.0).collect_vec(),
        );
        Ok(config)
    }

    /// Configures a lookup table based operation. These correspond to operations that are represented in
    /// the `circuit::eltwise` module.
    /// # Arguments
    ///
    /// * `node` - The [Node] must represent a lookup based op.
    /// * `meta` - Halo2 ConstraintSystem.
    /// * `vars` - [ModelVars] for the model.
    fn conf_table<F: FieldExt + TensorType>(
        &self,
        node: &Node,
        meta: &mut ConstraintSystem<F>,
        vars: &mut ModelVars<F>,
        tables: &mut BTreeMap<Vec<LookupOp>, Rc<RefCell<LookupTable<F>>>>,
    ) -> Result<NodeConfig<F>, Box<dyn Error>> {
        let input_len = node.in_dims[0].iter().product();
        let input = &vars.advices[0].reshape(&[input_len]);
        let output = &vars.advices[1].reshape(&[input_len]);
        let node_inputs = node.inputs.iter().map(|e| e.node).collect();

        let op = match &node.opkind {
            OpKind::Lookup(l) => l,
            c => {
                return Err(Box::new(GraphError::WrongMethod(node.idx, c.clone())));
            }
        };

        let config =
            if let std::collections::btree_map::Entry::Vacant(e) = tables.entry(vec![op.clone()]) {
                let conf: LookupConfig<F> =
                    LookupConfig::configure(meta, input, output, self.bits, &[op.clone()]);
                e.insert(conf.table.clone());
                NodeConfig::Lookup(conf, node_inputs)
            } else {
                let table = tables.get(&vec![op.clone()]).unwrap();
                let conf: LookupConfig<F> =
                    LookupConfig::configure_with_table(meta, input, output, table.clone());
                NodeConfig::Lookup(conf, node_inputs)
            };
        Ok(config)
    }

    /// Assigns values to the regions created when calling `configure`.
    /// # Arguments
    ///
    /// * `config` - [ModelConfig] holding all node configs.
    /// * `layouter` - Halo2 Layouter.
    /// * `inputs` - The values to feed into the circuit.
    pub fn layout<F: FieldExt + TensorType>(
        &self,
        config: ModelConfig<F>,
        layouter: &mut impl Layouter<F>,
        inputs: &[ValTensor<F>],
        vars: &ModelVars<F>,
    ) -> Result<(), Box<dyn Error>> {
        info!("model layout");
        let mut results = BTreeMap::<usize, ValTensor<F>>::new();
        for i in inputs.iter().enumerate() {
            if self.visibility.input.is_public() {
                results.insert(i.0, vars.instances[i.0].clone());
            } else {
                results.insert(i.0, i.1.clone());
            }
        }
        for (idx, config) in config.configs.iter() {
            if let Some(vt) = self.layout_config(layouter, &mut results, config)? {
                // we get the max as for fused nodes this corresponds to the node output
                results.insert(*idx, vt);
                //only use with mock prover
                if matches!(self.mode, Mode::Mock) {
                    trace!("------------ output {:?}", results.get(idx).unwrap().show());
                }
            }
        }

        let output_nodes = self.model.outputs.iter();
        info!(
            "model outputs are nodes: {:?}",
            output_nodes.clone().map(|o| o.node).collect_vec()
        );
        let outputs = output_nodes
            .map(|o| results.get(&o.node).unwrap().clone())
            .collect_vec();
        let _ = config
            .public_outputs
            .iter()
            .zip(outputs)
            .enumerate()
            .map(|(i, (range_check, output))| {
                let mut offset = 0;
                if self.visibility.input.is_public() {
                    offset += inputs.len();
                };
                range_check.layout(
                    layouter.namespace(|| "range check outputs"),
                    output,
                    vars.instances[offset + i].clone(),
                )
            })
            .collect_vec();
        info!("computing...");
        Ok(())
    }

    /// Assigns values to a single region, represented as a [NodeConfig].
    /// # Arguments
    ///
    /// * `config` - [NodeConfig] the single region we will layout.
    /// * `layouter` - Halo2 Layouter.
    /// * `inputs` - [BTreeMap] of values to feed into the [NodeConfig], can also include previous intermediate results, i.e the output of other nodes.
    fn layout_config<F: FieldExt + TensorType>(
        &self,
        layouter: &mut impl Layouter<F>,
        inputs: &mut BTreeMap<usize, ValTensor<F>>,
        config: &NodeConfig<F>,
    ) -> Result<Option<ValTensor<F>>, Box<dyn Error>> {
        // The node kind and the config should be the same.
        let res = match config.clone() {
            NodeConfig::Poly(mut ac, idx) => {
                let values: Vec<ValTensor<F>> = idx
                    .iter()
                    .map(|i| {
                        let node = &self.nodes.filter(*i);
                        match node.opkind {
                            OpKind::Const => {
                                let val = node
                                    .const_value
                                    .clone()
                                    .context("Tensor<i32> should already be loaded")
                                    .unwrap();
                                <Tensor<i32> as Into<Tensor<Value<F>>>>::into(val).into()
                            }
                            _ => inputs.get(i).unwrap().clone(),
                        }
                    })
                    .collect_vec();

                Some(ac.layout(layouter, &values)?)
            }
            NodeConfig::Lookup(rc, idx) => {
                if idx.len() != 1 {
                    return Err(Box::new(GraphError::InvalidLookupInputs));
                }
                // For activations and elementwise operations, the dimensions are sometimes only in one or the other of input and output.
                Some(rc.layout(layouter, inputs.get(&idx[0]).unwrap())?)
            }
            NodeConfig::Input => None,
            NodeConfig::Const => None,
            _ => {
                return Err(Box::new(GraphError::UnsupportedOp));
            }
        };
        Ok(res)
    }

    /// Iterates over Nodes and assigns execution buckets to them.  Each bucket holds either:
    /// a) independent lookup operations (i.e operations that don't feed into one another so can be processed in parallel).
    /// b) operations that can be fused together, i.e the output of one op might feed into another.
    /// The logic for bucket assignment is thus: we assign all data intake nodes to the 0 bucket.
    /// We iterate over each node in turn. If the node is a polynomial op, assign to it the maximum bucket of it's inputs.
    /// If the node is a lookup table, assign to it the maximum bucket of it's inputs incremented by 1.
    /// # Arguments
    ///
    /// * `nodes` - [BTreeMap] of (node index, [Node]) pairs.
    pub fn assign_execution_buckets(
        mut nodes: BTreeMap<usize, Node>,
    ) -> Result<NodeGraph, GraphError> {
        info!("assigning configuration buckets to operations");

        let mut bucketed_nodes = NodeGraph(BTreeMap::<Option<usize>, BTreeMap<usize, Node>>::new());

        for (_, node) in nodes.iter_mut() {
            let mut prev_buckets = vec![];
            for n in node
                .inputs
                .iter()
                .filter(|n| !bucketed_nodes.filter(n.node).opkind.is_const())
            {
                match bucketed_nodes.filter(n.node).bucket {
                    Some(b) => prev_buckets.push(b),
                    None => {
                        return Err(GraphError::MissingNode(n.node));
                    }
                }
            }
            let prev_bucket: Option<&usize> = prev_buckets.iter().max();

            match &node.opkind {
                OpKind::Input => node.bucket = Some(0),
                OpKind::Const => node.bucket = None,
                OpKind::Poly(_) => node.bucket = Some(*prev_bucket.unwrap()),
                OpKind::Lookup(_) => node.bucket = Some(prev_bucket.unwrap() + 1),
                op => {
                    return Err(GraphError::WrongMethod(node.idx, op.clone()));
                }
            }
            bucketed_nodes.insert(node.bucket, node.idx, node.clone());
        }

        Ok(bucketed_nodes)
    }

    /// Get a linear extension of the model (an evaluation order), for example to feed to circuit construction.
    /// Note that this order is not stable over multiple reloads of the model.  For example, it will freely
    /// interchange the order of evaluation of fixed parameters.   For example weight could have id 1 on one load,
    /// and bias id 2, and vice versa on the next load of the same file. The ids are also not stable.
    pub fn eval_order(&self) -> Result<Vec<usize>, AnyError> {
        self.model.eval_order()
    }

    /// Note that this order is not stable.
    pub fn nodes(&self) -> Vec<OnnxNode<InferenceFact, Box<dyn InferenceOp>>> {
        self.model.nodes().to_vec()
    }

    /// Returns the ID of the computational graph's inputs
    pub fn input_outlets(&self) -> Result<Vec<OutletId>, Box<dyn Error>> {
        Ok(self.model.input_outlets()?.to_vec())
    }

    /// Returns the ID of the computational graph's outputs
    pub fn output_outlets(&self) -> Result<Vec<OutletId>, Box<dyn Error>> {
        Ok(self.model.output_outlets()?.to_vec())
    }

    /// Returns the number of the computational graph's inputs
    pub fn num_inputs(&self) -> usize {
        let input_nodes = self.model.inputs.iter();
        input_nodes.len()
    }

    ///  Returns shapes of the computational graph's inputs
    pub fn input_shapes(&self) -> Vec<Vec<usize>> {
        self.model
            .inputs
            .iter()
            .map(|o| self.nodes.filter(o.node).out_dims)
            .collect_vec()
    }

    /// Returns the number of the computational graph's outputs
    pub fn num_outputs(&self) -> usize {
        let output_nodes = self.model.outputs.iter();
        output_nodes.len()
    }

    /// Returns shapes of the computational graph's outputs
    pub fn output_shapes(&self) -> Vec<Vec<usize>> {
        self.model
            .outputs
            .iter()
            .map(|o| self.nodes.filter(o.node).out_dims)
            .collect_vec()
    }

    /// Returns the fixed point scale of the computational graph's outputs
    pub fn get_output_scales(&self) -> Vec<i32> {
        let output_nodes = self.model.outputs.iter();
        output_nodes
            .map(|o| self.nodes.filter(o.node).out_scale)
            .collect_vec()
    }

    /// Max number of inlets or outlets to a node
    pub fn max_node_size(&self) -> usize {
        max(
            self.nodes
                .flatten()
                .iter()
                .map(|e| {
                    e.in_dims
                        .iter()
                        .map(|dims| dims.iter().product::<usize>())
                        .max()
                        .unwrap()
                })
                .max()
                .unwrap(),
            self.nodes
                .flatten()
                .iter()
                .map(|e| e.out_dims.iter().product())
                .max()
                .unwrap(),
        )
    }

    /// Max number of parameters (i.e trainable weights) across the computational graph
    pub fn max_node_params(&self) -> usize {
        let mut maximum_number_inputs = 0;
        for (_, bucket_nodes) in self.nodes.0.iter() {
            let fused_ops: BTreeMap<&usize, &Node> = bucket_nodes
                .iter()
                .filter(|(_, n)| n.opkind.is_poly())
                .collect();

            let params = fused_ops
                .iter()
                .flat_map(|(_, n)| n.inputs.iter().map(|o| o.node).collect_vec())
                // here we remove intermediary calculation / nodes within the layer
                .filter(|id| !fused_ops.contains_key(id))
                .filter(|id| self.nodes.filter(*id).opkind.is_const())
                .unique()
                .collect_vec();

            maximum_number_inputs = max(maximum_number_inputs, params.len());
        }
        // add 1 for layer output
        maximum_number_inputs + 1
    }

    /// Maximum number of input variables in fused layers
    pub fn max_node_vars_fused(&self) -> usize {
        let mut maximum_number_inputs = 0;
        for (_, bucket_nodes) in self.nodes.0.iter() {
            let fused_ops: BTreeMap<&usize, &Node> = bucket_nodes
                .iter()
                .filter(|(_, n)| n.opkind.is_poly())
                .collect();

            let fused_inputs = fused_ops
                .iter()
                .flat_map(|(_, n)| n.inputs.iter().map(|o| o.node).collect_vec())
                // here we remove intermediary calculation / nodes within the layer
                .filter(|id| !fused_ops.contains_key(id))
                .filter(|id| !self.nodes.filter(*id).opkind.is_const())
                .unique()
                .collect_vec();

            maximum_number_inputs = max(maximum_number_inputs, fused_inputs.len());
        }
        // add 1 for layer output
        maximum_number_inputs + 1
    }

    /// Maximum number of input variables in non-fused layers
    pub fn max_node_vars_non_fused(&self) -> usize {
        let mut maximum_number_inputs = 0;
        for (_, bucket_nodes) in self.nodes.0.iter() {
            let non_fused_ops = bucket_nodes
                .iter()
                .filter(|(_, n)| !n.opkind.is_poly())
                .map(|(_, n)| n.inputs.len())
                .max()
                .unwrap_or(0);

            maximum_number_inputs = max(maximum_number_inputs, non_fused_ops);
        }
        // add 1 for layer output
        maximum_number_inputs + 1
    }

    /// Number of instances used by the circuit
    pub fn num_instances(&self) -> (usize, Vec<Vec<usize>>) {
        // for now the number of instances corresponds to the number of graph / model outputs
        let mut num_instances = 0;
        let mut instance_shapes = vec![];
        if self.visibility.input.is_public() {
            num_instances += self.num_inputs();
            instance_shapes.extend(self.input_shapes());
        }
        if self.visibility.output.is_public() {
            num_instances += self.num_outputs();
            instance_shapes.extend(self.output_shapes());
        }
        (num_instances, instance_shapes)
    }

    /// Number of advice used by the circuit
    pub fn num_advice(&self) -> usize {
        // TODO: extract max number of params in a given fused layer
        if self.visibility.params.is_public() {
            // this is the maximum of variables in non-fused layer, and the maximum of variables (non-params) in fused layers
            max(self.max_node_vars_non_fused(), self.max_node_vars_fused())
        } else {
            // this is the maximum of variables in non-fused layer, and the maximum of variables (non-params) in fused layers
            //  + the max number of params in a fused layer
            max(
                self.max_node_vars_non_fused(),
                self.max_node_params() + self.max_node_vars_fused(),
            )
        }
    }

    /// Number of fixed columns used by the circuit
    pub fn num_fixed(&self) -> usize {
        let mut num_fixed = 0;
        if self.visibility.params.is_public() {
            num_fixed += self.max_node_params();
        }
        num_fixed
    }
}
