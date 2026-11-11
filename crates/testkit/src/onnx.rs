//! Tiny ONNX models for the embedder's tests, hand-encoded as the protobuf
//! bytes of an ONNX `ModelProto`, so the testkit needs no ONNX tooling and the
//! repo commits no model file.
//!
//! The field numbers are those of `onnx.proto` (ONNX 1.x): every message
//! below is built from varint fields (wire type 0) and length-delimited
//! fields (wire type 2), which is all the format needs here.

/// A model shaped like the registry's DINOv2 exports, with dims 3: the input
/// `pixel_values` is float `[N, 3, 224, 224]` (`N` symbolic), and the output
/// `last_hidden_state` is float `[N, 1, 3]`, one "token" per image holding the
/// mean of each channel: `ReduceMean(axes = [2, 3], keepdims = 0)`, then
/// `Unsqueeze(axes = [1])`.
///
/// Its embedding (the CLS row, L2-normalised) is therefore the normalised
/// vector of the per-channel means of the normalised pixels. IR version 7,
/// default-domain opset 11, where both operators still take their axes as
/// attributes.
pub fn cls_mean_model() -> Vec<u8> {
    model("cls_mean", &channel_mean_token("last_hidden_state"), 1)
}

/// [`cls_mean_model`] with a second token per image, the first one negated
/// (`Neg`, then `Concat(axis = 1)`): the output is `[N, 2, 3]`, and an
/// embedder that took any token but the first would flip every sign.
pub fn two_token_model() -> Vec<u8> {
    let mut nodes = channel_mean_token("cls");
    nodes.push(node("neg", "Neg", &["cls"], "negated", &[]));
    nodes.push(node(
        "concat",
        "Concat",
        &["cls", "negated"],
        "last_hidden_state",
        &[int_attribute("axis", 1)],
    ));
    model("two_tokens", &nodes, 2)
}

/// The nodes that turn `pixel_values` into `output`, `[N, 1, 3]`: the mean
/// of each channel as one token.
fn channel_mean_token(output: &str) -> Vec<Vec<u8>> {
    vec![
        node(
            "reduce_mean",
            "ReduceMean",
            &["pixel_values"],
            "channel_means",
            &[
                ints_attribute("axes", &[2, 3]),
                int_attribute("keepdims", 0),
            ],
        ),
        node(
            "unsqueeze",
            "Unsqueeze",
            &["channel_means"],
            output,
            &[ints_attribute("axes", &[1])],
        ),
    ]
}

/// The model of the graph `name` made of `nodes`, from `pixel_values` to
/// `last_hidden_state` of `[N, tokens, 3]`.
fn model(name: &str, nodes: &[Vec<u8>], tokens: i64) -> Vec<u8> {
    let input = value_info(
        "pixel_values",
        &[
            Dim::Param("N"),
            Dim::Value(3),
            Dim::Value(224),
            Dim::Value(224),
        ],
    );
    let output = value_info(
        "last_hidden_state",
        &[Dim::Param("N"), Dim::Value(tokens), Dim::Value(3)],
    );

    let mut graph = Vec::new();
    for node in nodes {
        message_field(&mut graph, GRAPH_NODE, node);
    }
    string_field(&mut graph, GRAPH_NAME, name);
    message_field(&mut graph, GRAPH_INPUT, &input);
    message_field(&mut graph, GRAPH_OUTPUT, &output);

    let mut opset = Vec::new();
    string_field(&mut opset, OPSET_DOMAIN, "");
    int_field(&mut opset, OPSET_VERSION, 11);

    let mut model = Vec::new();
    int_field(&mut model, MODEL_IR_VERSION, 7);
    string_field(&mut model, MODEL_PRODUCER_NAME, "photostacker-testkit");
    message_field(&mut model, MODEL_GRAPH, &graph);
    message_field(&mut model, MODEL_OPSET_IMPORT, &opset);
    model
}

// ModelProto
const MODEL_IR_VERSION: u32 = 1;
const MODEL_PRODUCER_NAME: u32 = 2;
const MODEL_GRAPH: u32 = 7;
const MODEL_OPSET_IMPORT: u32 = 8;
// OperatorSetIdProto
const OPSET_DOMAIN: u32 = 1;
const OPSET_VERSION: u32 = 2;
// GraphProto
const GRAPH_NODE: u32 = 1;
const GRAPH_NAME: u32 = 2;
const GRAPH_INPUT: u32 = 11;
const GRAPH_OUTPUT: u32 = 12;
// NodeProto
const NODE_INPUT: u32 = 1;
const NODE_OUTPUT: u32 = 2;
const NODE_NAME: u32 = 3;
const NODE_OP_TYPE: u32 = 4;
const NODE_ATTRIBUTE: u32 = 5;
// AttributeProto, and its AttributeType values
const ATTRIBUTE_NAME: u32 = 1;
const ATTRIBUTE_I: u32 = 3;
const ATTRIBUTE_INTS: u32 = 8;
const ATTRIBUTE_TYPE: u32 = 20;
const ATTRIBUTE_TYPE_INT: i64 = 2;
const ATTRIBUTE_TYPE_INTS: i64 = 7;
// ValueInfoProto
const VALUE_INFO_NAME: u32 = 1;
const VALUE_INFO_TYPE: u32 = 2;
// TypeProto, TypeProto.Tensor (and TensorProto.DataType FLOAT)
const TYPE_TENSOR_TYPE: u32 = 1;
const TENSOR_ELEM_TYPE: u32 = 1;
const TENSOR_SHAPE: u32 = 2;
const DATA_TYPE_FLOAT: i64 = 1;
// TensorShapeProto, and its Dimension
const SHAPE_DIM: u32 = 1;
const DIM_VALUE: u32 = 1;
const DIM_PARAM: u32 = 2;

/// The protobuf wire types used here.
const WIRE_VARINT: u64 = 0;
const WIRE_LENGTH_DELIMITED: u64 = 2;

/// One dimension of a tensor shape: fixed, or a named symbolic one.
enum Dim {
    Value(i64),
    Param(&'static str),
}

/// A node with one output.
fn node(
    name: &str,
    op_type: &str,
    inputs: &[&str],
    output: &str,
    attributes: &[Vec<u8>],
) -> Vec<u8> {
    let mut node = Vec::new();
    for input in inputs {
        string_field(&mut node, NODE_INPUT, input);
    }
    string_field(&mut node, NODE_OUTPUT, output);
    string_field(&mut node, NODE_NAME, name);
    string_field(&mut node, NODE_OP_TYPE, op_type);
    for attribute in attributes {
        message_field(&mut node, NODE_ATTRIBUTE, attribute);
    }
    node
}

fn int_attribute(name: &str, value: i64) -> Vec<u8> {
    let mut attribute = Vec::new();
    string_field(&mut attribute, ATTRIBUTE_NAME, name);
    int_field(&mut attribute, ATTRIBUTE_I, value);
    int_field(&mut attribute, ATTRIBUTE_TYPE, ATTRIBUTE_TYPE_INT);
    attribute
}

/// The repeated `ints` are written unpacked, one field per value, which every
/// protobuf parser reads.
fn ints_attribute(name: &str, values: &[i64]) -> Vec<u8> {
    let mut attribute = Vec::new();
    string_field(&mut attribute, ATTRIBUTE_NAME, name);
    for &value in values {
        int_field(&mut attribute, ATTRIBUTE_INTS, value);
    }
    int_field(&mut attribute, ATTRIBUTE_TYPE, ATTRIBUTE_TYPE_INTS);
    attribute
}

/// A float tensor named `name` of shape `dims`.
fn value_info(name: &str, dims: &[Dim]) -> Vec<u8> {
    let mut shape = Vec::new();
    for dim in dims {
        let mut dimension = Vec::new();
        match dim {
            Dim::Value(value) => int_field(&mut dimension, DIM_VALUE, *value),
            Dim::Param(param) => string_field(&mut dimension, DIM_PARAM, param),
        }
        message_field(&mut shape, SHAPE_DIM, &dimension);
    }
    let mut tensor = Vec::new();
    int_field(&mut tensor, TENSOR_ELEM_TYPE, DATA_TYPE_FLOAT);
    message_field(&mut tensor, TENSOR_SHAPE, &shape);
    let mut type_proto = Vec::new();
    message_field(&mut type_proto, TYPE_TENSOR_TYPE, &tensor);

    let mut value_info = Vec::new();
    string_field(&mut value_info, VALUE_INFO_NAME, name);
    message_field(&mut value_info, VALUE_INFO_TYPE, &type_proto);
    value_info
}

/// An int32, int64 or enum field. Negative values would take ten bytes, as
/// protobuf encodes them; none is used here.
fn int_field(out: &mut Vec<u8>, field: u32, value: i64) {
    varint(out, (u64::from(field) << 3) | WIRE_VARINT);
    varint(out, value as u64);
}

fn string_field(out: &mut Vec<u8>, field: u32, value: &str) {
    message_field(out, field, value.as_bytes());
}

/// A length-delimited field: an embedded message, or a string's bytes.
fn message_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    varint(out, (u64::from(field) << 3) | WIRE_LENGTH_DELIMITED);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Base-128, least significant group first, the high bit marking "more".
fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The top-level fields of a protobuf message, as (field number, wire
    /// type, varint value or payload); panics on malformed bytes.
    fn fields(mut bytes: &[u8]) -> Vec<(u64, u64, u64, &[u8])> {
        fn read_varint(bytes: &mut &[u8]) -> u64 {
            let mut value = 0;
            for shift in (0..64).step_by(7) {
                let (&byte, rest) = bytes.split_first().expect("a varint ends");
                *bytes = rest;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
            }
            panic!("a varint is at most ten bytes");
        }
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let key = read_varint(&mut bytes);
            let (field, wire) = (key >> 3, key & 7);
            match wire {
                WIRE_VARINT => out.push((field, wire, read_varint(&mut bytes), &[][..])),
                WIRE_LENGTH_DELIMITED => {
                    let len = read_varint(&mut bytes) as usize;
                    let (payload, rest) = bytes.split_at(len);
                    bytes = rest;
                    out.push((field, wire, 0, payload));
                }
                other => panic!("unexpected wire type {other}"),
            }
        }
        out
    }

    #[test]
    fn varints_take_seven_bits_a_byte() {
        let mut out = Vec::new();
        varint(&mut out, 1);
        varint(&mut out, 300);
        assert_eq!(out, [0x01, 0xac, 0x02]);
    }

    #[test]
    fn the_model_is_a_well_formed_model_proto() {
        let model = cls_mean_model();
        // ir_version = 7 comes first.
        assert_eq!(model[..2], [0x08, 0x07]);

        let top = fields(&model);
        let numbers: Vec<u64> = top.iter().map(|field| field.0).collect();
        assert_eq!(numbers, [1, 2, 7, 8]);
        assert_eq!(top[0].2, 7);
        assert_eq!(top[1].3, b"photostacker-testkit");

        let opset = fields(top[3].3);
        assert_eq!((opset[0].0, opset[0].3), (1, &b""[..]));
        assert_eq!((opset[1].0, opset[1].2), (2, 11));

        let graph = fields(top[2].3);
        let numbers: Vec<u64> = graph.iter().map(|field| field.0).collect();
        assert_eq!(numbers, [1, 1, 2, 11, 12]);
        let op_types: Vec<&[u8]> = graph[..2]
            .iter()
            .map(|node| fields(node.3).into_iter().find(|f| f.0 == 4).unwrap().3)
            .collect();
        assert_eq!(op_types, [&b"ReduceMean"[..], b"Unsqueeze"]);
    }

    #[test]
    fn the_two_token_model_negates_a_second_token() {
        let model = two_token_model();
        let top = fields(&model);
        let graph = fields(top[2].3);
        let op_types: Vec<&[u8]> = graph
            .iter()
            .filter(|field| field.0 == 1)
            .map(|node| fields(node.3).into_iter().find(|f| f.0 == 4).unwrap().3)
            .collect();
        assert_eq!(
            op_types,
            [&b"ReduceMean"[..], b"Unsqueeze", b"Neg", b"Concat"]
        );
        // The Concat node reads both tokens.
        let concat = fields(graph[3].3);
        let inputs: Vec<&[u8]> = concat.iter().filter(|f| f.0 == 1).map(|f| f.3).collect();
        assert_eq!(inputs, [&b"cls"[..], b"negated"]);
    }
}
