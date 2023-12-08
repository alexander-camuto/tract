use tract_core::internal::*;
use tract_core::ops::array::{Pad, PadMode};
use tract_core::ops::binary::wire_with_rank_broadcast;
use tract_core::ops::cnn::KernelFormat;
use tract_core::ops::cnn::{ConvUnary, PaddingSpec};
use tract_core::ops::einsum::BasicMatMul;
use tract_core::ops::element_wise::ElementWiseOp;
use tract_core::ops::math::{add, sub, Recip};
use tract_core::ops::nn::{DataFormat, Softmax};
use tract_core::tract_data::itertools::Itertools;

pub fn rewrite_for_tflite(model: &mut TypedModel) -> TractResult<()> {
    tract_core::ops::einsum::rewrite_einsums_as_matmul(model)?;
    Rewriter::default()
        .with_rule_for("trivial_axes_around_matmul", trivial_axes_around_matmul)
        .with_rule_for("kernel_in_ohwi", kernel_in_ohwi)
        .with_rule_for("bias_as_vector", bias_as_vector)
        .with_rule_for("homogeneous-convolution", homogeneous_convolution)
        .with_rule_for("make_1d_2d", make_1d_2d)
        .with_rule_for("force_n_axis", force_n_axis)
        .with_rule_for("nchw-to-nhwc", nchw_to_nhwc)
        .with_rule_for("padding", padding)
        .with_rule_for("manual_recip", manual_recip)
        .with_rule_for("softmax_on_last_axis", softmax_on_last_axis)
        .rewrite(&(), model)
}

fn trivial_axes_around_matmul(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &BasicMatMul,
) -> TractResult<Option<TypedModelPatch>> {
    let facts = model.node_input_facts(node.id)?;
    let rank = facts[0].rank();
    if rank <= 4 {
        return Ok(None);
    }
    let trivial_axes = (0..rank - 2)
        .filter(|axis| facts[0].shape[*axis].is_one() && facts[1].shape[*axis].is_one())
        .collect_vec();
    let mut patch = TypedModelPatch::default();
    let mut wire = patch.taps(model, &node.inputs)?;
    for axis in trivial_axes.iter().rev() {
        wire[0] =
            patch.wire_node(format!("{name}.rm_a_axis_{axis}"), AxisOp::Rm(*axis), &[wire[0]])?[0];
        wire[1] =
            patch.wire_node(format!("{name}.rm_b_axis_{axis}"), AxisOp::Rm(*axis), &[wire[1]])?[0];
    }
    let mut out = patch.wire_node(&node.name, conv.clone(), &wire)?;
    for axis in trivial_axes {
        out = patch.wire_node(format!("{name}.add_axis_{axis}"), AxisOp::Add(axis), &out)?;
    }
    patch.shunt_outside(model, node.id.into(), out[0])?;
    Ok(Some(patch))
}

fn kernel_in_ohwi(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &ConvUnary,
) -> TractResult<Option<TypedModelPatch>> {
    if conv.kernel_fmt == KernelFormat::OHWI {
        return Ok(None);
    }
    if conv.group != 1 && conv.group != conv.output_channels() {
        bail!("Arbitrary grouping is not supported in tflite")
    }
    let mut patch = TypedModelPatch::default();
    let mut wire = patch.taps(model, &node.inputs)?;
    let prefix = format!("{name}.kernel_reorg");
    for (ix, op) in conv
        .kernel_fmt
        .kernel_as_group_o_i_h_w_ops(&patch.outlet_fact(wire[1])?.shape, conv.group)
        .into_iter()
        .enumerate()
    {
        wire[1] = patch.wire_node(format!("{prefix}.{ix}"), op, &[wire[1]])?[0];
    }
    let geo_rank = conv.pool_spec.kernel_shape.len();
    // group_o_i_h_w -> o_h_w_gi
    let ci = conv.input_channels();
    wire[1] =
        patch.wire_node(format!("{prefix}.mv_g"), AxisOp::Move(0, geo_rank + 2), &[wire[1]])?[0];
    wire[1] =
        patch.wire_node(format!("{prefix}.mv_i"), AxisOp::Move(1, geo_rank + 2), &[wire[1]])?[0];
    wire[1] = patch.wire_node(
        format!("{prefix}.gi"),
        AxisOp::Reshape(
            geo_rank + 1,
            tvec!(conv.group.to_dim(), (ci / conv.group).to_dim()),
            tvec!(ci.to_dim()),
        ),
        &[wire[1]],
    )?[0];
    let new = ConvUnary { kernel_fmt: KernelFormat::OHWI, ..conv.clone() };
    wire = patch.wire_node(name, new, &wire)?;
    patch.shunt_outside(model, node.id.into(), wire[0])?;
    Ok(Some(patch))
}

fn bias_as_vector(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &ConvUnary,
) -> TractResult<Option<TypedModelPatch>> {
    let bias_fact = model.outlet_fact(node.inputs[2])?;
    let co = conv.output_channels();
    if *bias_fact.shape == [co.to_dim()] {
        return Ok(None);
    }
    let mut patch = TypedModelPatch::default();
    let mut wire = patch.taps(model, &node.inputs)?;
    wire[2] = tract_core::ops::cnn::wire_reshape_bias_as_vector(
        &mut patch,
        name,
        wire[2],
        conv.output_channels(),
    )?[0];
    wire = patch.wire_node(name, conv.clone(), &wire)?;
    patch.shunt_outside(model, node.id.into(), wire[0])?;
    Ok(Some(patch))
}

fn homogeneous_convolution(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &ConvUnary,
) -> TractResult<Option<TypedModelPatch>> {
    let input_fact = model.outlet_fact(node.inputs[0])?;
    let idt = input_fact.datum_type;
    let kernel_fact = model.outlet_fact(node.inputs[1])?;
    let kdt = kernel_fact.datum_type;
    match (kdt.unquantized(), idt.unquantized()) {
        (DatumType::I8, DatumType::U8) => {
            todo!()
            /*
            let new_qp =
            QParams::ZpScale { zero_point: kdt.zp_scale().0 + 128, scale: kdt.zp_scale().1 };
            let new_dt = u8::datum_type().quantize(new_qp);
            let mut new_kernel = conv.kernel.clone().into_tensor();
            unsafe {
            new_kernel
            .as_slice_mut_unchecked::<u8>()
            .iter_mut()
            .for_each(|x| *x = x.wrapping_add(128));
            new_kernel.set_datum_type(new_dt)
            }
            let mut new = conv.clone();
            new.kernel = new_kernel.into_arc_tensor();
            let mut patch = TypedModelPatch::default();
            let mut wire = patch.taps(model, &node.inputs)?;
            let k0_fix = patch.add_const(format!("{name}.128"), tensor0(128i32))?;
            wire[1] = wire_with_rank_broadcast(
            format!("{name}.fix_k0"),
            &mut patch,
            add(),
            &[wire[1], k0_fix],
            )?[0];
            wire = patch.wire_node(name, new, &wire)?;
            patch.shunt_outside(model, node.id.into(), wire[0])?;
            Ok(Some(patch))
            */
        }
        (DatumType::U8, DatumType::I8) => {
            todo!()
            /*
            let new_qp =
            QParams::ZpScale { zero_point: kdt.zp_scale().0 - 128, scale: kdt.zp_scale().1 };
            let new_dt = i8::datum_type().quantize(new_qp);
            let mut new_kernel = conv.kernel.clone().into_tensor();
            unsafe {
            new_kernel
            .as_slice_mut_unchecked::<u8>()
            .iter_mut()
            .for_each(|x| *x = x.wrapping_sub(128));
            new_kernel.set_datum_type(new_dt)
            }
            let mut new = conv.clone();
            new.kernel = new_kernel.into_arc_tensor();
            let mut patch = TypedModelPatch::default();
            let mut wire = patch.taps(model, &node.inputs)?;
            let k0_fix = patch.add_const(format!("{name}.128"), tensor0(128i32))?;
            wire[1] = wire_with_rank_broadcast(
            format!("{name}.fix_k0"),
            &mut patch,
            sub(),
            &[wire[1], k0_fix],
            )?[0];
            wire = patch.wire_node(name, new, &wire)?;
            patch.shunt_outside(model, node.id.into(), wire[0])?;
            Ok(Some(patch))
            */
        }
        _ => Ok(None),
    }
}

fn force_n_axis(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &ConvUnary,
) -> TractResult<Option<TypedModelPatch>> {
    if !conv.pool_spec.data_format.has_n() {
        let mut new = conv.clone();
        new.pool_spec.data_format = conv.pool_spec.data_format.with_n();
        let mut patch = TypedModelPatch::default();
        let mut wire = patch.taps(model, &node.inputs)?;
        wire[0] = patch.wire_node(format!("{name}.add_n"), AxisOp::Add(0), &[wire[0]])?[0];
        wire = patch.wire_node(name, new, &wire)?;
        wire = patch.wire_node(format!("{name}.rm_n"), AxisOp::Rm(0), &wire)?;
        patch.shunt_outside(model, node.id.into(), wire[0])?;
        return Ok(Some(patch));
    }
    Ok(None)
}

fn make_1d_2d(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &ConvUnary,
) -> TractResult<Option<TypedModelPatch>> {
    if conv.pool_spec.rank() == 1 {
        let mut new = conv.clone();
        new.pool_spec = conv.pool_spec.change_geo_axes(&AxisOp::Add(1))?;
        let mut patch = TypedModelPatch::default();
        let mut wire = patch.taps(model, &node.inputs)?;
        let pos_data = conv.pool_spec.data_format.h_axis() + 1;
        wire[0] = patch.wire_node(format!("{name}.add_dim"), AxisOp::Add(pos_data), &[wire[0]])?[0];
        let pos_kernel = conv.kernel_fmt.h_axis() + 1;
        wire[1] =
            patch.wire_node(format!("{name}.add_dim_k"), AxisOp::Add(pos_kernel), &[wire[1]])?[0];
        wire = patch.wire_node(name, new, &wire)?;
        wire = patch.wire_node(format!("{name}.rm_dim"), AxisOp::Rm(pos_data), &wire)?;
        patch.shunt_outside(model, node.id.into(), wire[0])?;
        return Ok(Some(patch));
    }
    Ok(None)
}

fn nchw_to_nhwc(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &ConvUnary,
) -> TractResult<Option<TypedModelPatch>> {
    if !conv.pool_spec.data_format.c_is_last() {
        let mut new = conv.clone();
        new.pool_spec.data_format = match conv.pool_spec.data_format {
            DataFormat::NHWC | DataFormat::HWC => unreachable!(),
            DataFormat::CHW => DataFormat::HWC,
            DataFormat::NCHW => DataFormat::NHWC,
        };
        let mut patch = TypedModelPatch::default();
        let fact = model.outlet_fact(node.inputs[0])?;
        let shape = conv.pool_spec.data_format.shape(&fact.shape)?;
        let before = shape.c_axis();
        let after = fact.rank() - 1;
        let mut wire = patch.taps(model, &node.inputs)?;
        wire[0] =
            patch.wire_node(format!("{name}.nhwc"), AxisOp::Move(before, after), &[wire[0]])?[0];
        wire = patch.wire_node(name, new, &wire)?;
        wire = patch.wire_node(format!("{name}.nchw"), AxisOp::Move(after, before), &wire)?;
        patch.shunt_outside(model, node.id.into(), wire[0])?;
        return Ok(Some(patch));
    }
    Ok(None)
}

fn padding(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    conv: &ConvUnary,
) -> TractResult<Option<TypedModelPatch>> {
    if conv.pool_spec.padding != PaddingSpec::Valid
    // FIXME SameUpper should be usable, but I can't make sense of tflite output
    // && conv.pool_spec.padding != PaddingSpec::SameUpper
    {
        let fact = model.outlet_fact(node.inputs[0])?;
        let shape = conv.pool_spec.data_format.shape(&fact.shape)?;
        let actual = conv.pool_spec.computed_padding(shape.hw_dims());
        #[allow(clippy::single_element_loop)]
        for pad in [PaddingSpec::Valid /*, PaddingSpec::SameUpper*/] {
            let found = pad.compute(
                shape.hw_dims(),
                &conv.pool_spec.kernel_shape,
                &conv.pool_spec.dilations(),
                &conv.pool_spec.strides(),
            );
            if actual == found {
                let mut new = conv.clone();
                new.pool_spec.padding = pad;
                return Ok(Some(TypedModelPatch::replace_single_op(
                    model,
                    node,
                    &node.inputs,
                    new,
                )?));
            }
        }
        let mut patch = TypedModelPatch::default();
        let mut wires = patch.taps(model, &node.inputs)?;
        let mut pads = vec![(0usize, 0usize); fact.rank()];
        for (padding, axis) in actual.iter().zip(shape.hw_axes()) {
            pads[axis] = (padding.pad_before.to_usize()?, padding.pad_after.to_usize()?);
        }
        wires[0] = patch.wire_node(
            format!("{name}.padding"),
            Pad {
                pads,
                mode: PadMode::Constant(Tensor::zero_scalar_dt(fact.datum_type)?.into_arc_tensor()),
            },
            &wires[0..1],
        )?[0];
        let mut new = conv.clone();
        new.pool_spec.padding = PaddingSpec::Valid;
        wires = patch.wire_node(&node.name, new, &wires)?;
        patch.shunt_outside(model, node.id.into(), wires[0])?;
        return Ok(Some(patch));
    }
    Ok(None)
}

fn manual_recip(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    recip: &ElementWiseOp,
) -> TractResult<Option<TypedModelPatch>> {
    if recip.0.is::<Recip>() {
        let mut patch = TypedModelPatch::default();
        let input = patch.tap_model(model, node.inputs[0])?;
        let dt = model.outlet_fact(node.inputs[0])?.datum_type;
        let one = tensor0(1i32).cast_to_dt(dt)?.into_owned().into_tensor();
        let one = patch.add_const(format!("{name}.one"), one)?;
        let wire = wire_with_rank_broadcast(
            name,
            &mut patch,
            tract_core::ops::math::div(),
            &[one, input],
        )?;
        patch.shunt_outside(model, node.id.into(), wire[0])?;
        Ok(Some(patch))
    } else {
        Ok(None)
    }
}

fn softmax_on_last_axis(
    _ctx: &(),
    model: &TypedModel,
    node: &TypedNode,
    name: &str,
    softmax: &Softmax,
) -> TractResult<Option<TypedModelPatch>> {
    let rank = model.outlet_fact(node.inputs[0])?.rank();
    ensure!(softmax.axes.len() == 1);
    if softmax.axes[0] != rank - 1 {
        let mut patch = TypedModelPatch::default();
        let mut wire = tvec!(patch.tap_model(model, node.inputs[0])?);
        wire = patch.wire_node(
            format!("{name}.move_axis"),
            AxisOp::Move(softmax.axes[0], rank - 1),
            &wire,
        )?;
        wire = patch.wire_node(
            format!("{name}.softmax"),
            Softmax { axes: tvec!(rank - 1), ..*softmax },
            &wire,
        )?;
        wire = patch.wire_node(
            format!("{name}.move_axis_back"),
            AxisOp::Move(rank - 1, softmax.axes[0]),
            &wire,
        )?;
        patch.shunt_outside(model, node.id.into(), wire[0])?;
        Ok(Some(patch))
    } else {
        Ok(None)
    }
}
