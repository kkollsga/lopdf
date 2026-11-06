use super::{Object, Stream};
use crate::Result;
pub use crate::parser::ContentOperations;
use crate::writer::Writer;
use std::io::Write;

#[derive(Debug, Clone)]
pub struct Operation {
    pub operator: String,
    pub operands: Vec<Object>,
}

impl Operation {
    pub fn new(operator: &str, operands: Vec<Object>) -> Operation {
        Operation {
            operator: operator.to_string(),
            operands,
        }
    }

    /// Conservative retained allocation weight for this operation and its
    /// recursively owned operands.
    pub fn retained_bytes(&self) -> u64 {
        let bytes = std::mem::size_of::<Self>()
            .saturating_add(self.operator.capacity())
            .saturating_add(self.operands.capacity().saturating_mul(std::mem::size_of::<Object>()))
            .saturating_add(
                self.operands
                    .iter()
                    .map(Object::retained_heap_bytes)
                    .fold(0, usize::saturating_add),
            );
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone)]
pub struct Content<Operations: AsRef<[Operation]> = Vec<Operation>> {
    pub operations: Operations,
}

impl<Operations: AsRef<[Operation]>> Content<Operations> {
    /// Encode content operations.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();
        let mut first_operation = true;
        for operation in self.operations.as_ref() {
            // Add new line after each operation except the last one.
            if first_operation {
                first_operation = false;
            } else {
                buffer.write_all(b"\n")?;
            }
            // A decoded inline image is a BI operation holding the image as a stream.
            if let ("BI", [Object::Stream(image)]) = (operation.operator.as_str(), operation.operands.as_slice()) {
                encode_inline_image(&mut buffer, image)?;
                continue;
            }
            for operand in &operation.operands {
                Writer::write_object(&mut buffer, operand)?;
                buffer.write_all(b" ")?;
            }
            buffer.write_all(operation.operator.as_bytes())?;
        }
        Ok(buffer)
    }
}

/// Write an inline image as `BI`, its dictionary entries, `ID`, its data and `EI`.
fn encode_inline_image(buffer: &mut Vec<u8>, image: &Stream) -> Result<()> {
    buffer.write_all(b"BI")?;
    for (key, value) in &image.dict {
        // Readers find the end of the data from the image size, and `Stream::new` adds a
        // Length the source dictionary did not have.
        if key == b"Length" {
            continue;
        }
        buffer.write_all(b" ")?;
        Writer::write_name(buffer, key)?;
        buffer.write_all(b" ")?;
        Writer::write_object(buffer, value)?;
    }
    buffer.write_all(b" ID ")?;
    buffer.write_all(&image.content)?;
    buffer.write_all(b"\nEI")?;
    Ok(())
}

impl Content<Vec<Operation>> {
    /// Conservative retained allocation weight for a decoded content AST.
    ///
    /// Unused operation and operand capacity is included so downstream budget
    /// reconciliation does not treat spare allocator capacity as free.
    pub fn retained_bytes(&self) -> u64 {
        let bytes = std::mem::size_of::<Self>()
            .saturating_add(
                self.operations
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Operation>()),
            )
            .saturating_add(
                self.operations
                    .iter()
                    .map(|operation| {
                        operation
                            .operator
                            .capacity()
                            .saturating_add(
                                operation
                                    .operands
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<Object>()),
                            )
                            .saturating_add(
                                operation
                                    .operands
                                    .iter()
                                    .map(Object::retained_heap_bytes)
                                    .fold(0, usize::saturating_add),
                            )
                    })
                    .fold(0, usize::saturating_add),
            );
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod retained_weight_tests {
    use std::mem::size_of;

    use super::{Content, Operation};
    use crate::{Object, StringFormat};

    #[test]
    fn retained_weight_tracks_operator_operand_and_operation_capacity() {
        let mut operator = String::with_capacity(128);
        operator.push_str("Tj");
        let mut text = Vec::with_capacity(256);
        text.extend_from_slice(b"hello");
        let mut operands = Vec::with_capacity(16);
        operands.push(Object::String(text, StringFormat::Literal));
        let operation = Operation { operator, operands };
        let compact = Operation::new("Tj", vec![Object::string_literal(b"hello".to_vec())]);

        assert!(operation.retained_bytes() > compact.retained_bytes());

        let mut operations = Vec::with_capacity(8);
        operations.push(operation);
        let content = Content { operations };
        let compact_content = Content {
            operations: vec![compact],
        };
        assert!(content.retained_bytes() > compact_content.retained_bytes());
        assert!(
            content.retained_bytes()
                >= u64::try_from(
                    size_of::<Content<Vec<Operation>>>()
                        + 8 * size_of::<Operation>()
                        + 128
                        + 16 * size_of::<Object>()
                        + 256,
                )
                .unwrap()
        );
    }
}
