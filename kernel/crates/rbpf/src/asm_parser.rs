// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Copyright 2017 Rich Lane <lanerl@gmail.com>

// Rust-doc comments were left in the module, but it is no longer publicly exposed from the root
// file of the crate. Do not expect to find those comments in the documentation of the crate.

//! This module parses eBPF assembly language source code.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

#[cfg(feature = "std")]
use combine::EasyParser;
use combine::{
    attempt, between, eof, many, many1, one_of, optional,
    parser::char::{alpha_num, char, digit, hex_digit, spaces, string},
    sep_by,
    stream::position::{self},
    ParseError, Parser, Stream,
};

/// Operand of an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    /// Register number.
    Register(i64),
    /// Jump offset or immediate.
    Integer(i64),
    /// Register number and offset.
    Memory(i64, i64),
    /// Used for pattern matching.
    Nil,
}

/// Parsed instruction.
#[derive(Debug, PartialEq, Eq)]
pub struct Instruction {
    /// Instruction name.
    pub name: String,
    /// Operands.
    pub operands: Vec<Operand>,
}

fn ident<I>() -> impl Parser<I, Output = String>
where
    I: Stream<Token = char>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    many1(alpha_num())
}

fn integer<I>() -> impl Parser<I, Output = i64>
where
    I: Stream<Token = char>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    let sign = optional(one_of("-+".chars())).map(|x| match x {
        Some('-') => -1,
        _ => 1,
    });
    let hex = string("0x")
        .with(many1(hex_digit()))
        .map(|x: String| u64::from_str_radix(&x, 16).unwrap() as i64);
    let dec = many1(digit()).map(|x: String| x.parse::<i64>().unwrap());
    (sign, attempt(hex).or(dec)).map(|(s, x)| s * x)
}

fn register<I>() -> impl Parser<I, Output = i64>
where
    I: Stream<Token = char>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    char('r')
        .with(many1(digit()))
        .map(|x: String| x.parse::<i64>().unwrap())
}

fn operand<I>() -> impl Parser<I, Output = Operand>
where
    I: Stream<Token = char>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    let register_operand = register().map(Operand::Register);
    let immediate = integer().map(Operand::Integer);
    let memory = between(char('['), char(']'), (register(), optional(integer())))
        .map(|t| Operand::Memory(t.0, t.1.unwrap_or(0)));
    register_operand.or(immediate).or(memory)
}

fn instruction<I>() -> impl Parser<I, Output = Instruction>
where
    I: Stream<Token = char>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    let operands = sep_by(operand(), char(',').skip(spaces()));
    (ident().skip(spaces()), operands, spaces()).map(|t| Instruction {
        name: t.0,
        operands: t.1,
    })
}

/// Parse a string into a list of instructions.
///
/// The instructions are not validated and may have invalid names and operand types.
pub fn parse(input: &str) -> Result<Vec<Instruction>, String> {
    let mut with = spaces().with(many(instruction()).skip(eof()));

    #[cfg(feature = "std")]
    {
        match with.easy_parse(position::Stream::new(input)) {
            Ok((insts, _)) => Ok(insts),
            Err(err) => Err(err.to_string()),
        }
    }
    #[cfg(not(feature = "std"))]
    {
        match with.parse(position::Stream::new(input)) {
            Ok((insts, _)) => Ok(insts),
            Err(err) => Err(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use combine::Parser;

    use super::{ident, instruction, integer, operand, parse, register, Instruction, Operand};

    // Unit tests for the different kinds of parsers.

    #[test]
    fn test_ident() {
        assert_eq!(ident().parse("nop"), Ok(("nop".to_string(), "")));
        assert_eq!(ident().parse("add32"), Ok(("add32".to_string(), "")));
        assert_eq!(ident().parse("add32*"), Ok(("add32".to_string(), "*")));
    }

    #[test]
    fn test_integer() {
        assert_eq!(integer().parse("0"), Ok((0, "")));
        assert_eq!(integer().parse("42"), Ok((42, "")));
        assert_eq!(integer().parse("+42"), Ok((42, "")));
        assert_eq!(integer().parse("-42"), Ok((-42, "")));
        assert_eq!(integer().parse("0x0"), Ok((0, "")));
        assert_eq!(
            integer().parse("0x123456789abcdef0"),
            Ok((0x123456789abcdef0, ""))
        );
        assert_eq!(integer().parse("-0x1f"), Ok((-31, "")));
    }

    #[test]
    fn test_register() {
        assert_eq!(register().parse("r0"), Ok((0, "")));
        assert_eq!(register().parse("r15"), Ok((15, "")));
    }

    #[test]
    fn test_operand() {
        assert_eq!(operand().parse("r0"), Ok((Operand::Register(0), "")));
        assert_eq!(operand().parse("r15"), Ok((Operand::Register(15), "")));
        assert_eq!(operand().parse("0"), Ok((Operand::Integer(0), "")));
        assert_eq!(operand().parse("42"), Ok((Operand::Integer(42), "")));
        assert_eq!(operand().parse("[r1]"), Ok((Operand::Memory(1, 0), "")));
        assert_eq!(operand().parse("[r3+5]"), Ok((Operand::Memory(3, 5), "")));
        assert_eq!(
            operand().parse("[r3+0x1f]"),
            Ok((Operand::Memory(3, 31), ""))
        );
        assert_eq!(
            operand().parse("[r3-0x1f]"),
            Ok((Operand::Memory(3, -31), ""))
        );
    }

    #[test]
    fn test_instruction() {
        assert_eq!(
            instruction().parse("exit"),
            Ok((
                Instruction {
                    name: "exit".to_string(),
                    operands: vec![],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("call 2"),
            Ok((
                Instruction {
                    name: "call".to_string(),
                    operands: vec![Operand::Integer(2)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("addi r1, 2"),
            Ok((
                Instruction {
                    name: "addi".to_string(),
                    operands: vec![Operand::Register(1), Operand::Integer(2)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("ldxb r2, [r1+12]"),
            Ok((
                Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 12)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("lsh r3, 0x8"),
            Ok((
                Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(8)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("jne r3, 0x8, +37"),
            Ok((
                Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(3),
                        Operand::Integer(8),
                        Operand::Integer(37)
                    ],
                },
                ""
            ))
        );

        // Whitespace between operands is optional.
        assert_eq!(
            instruction().parse("jne r3,0x8,+37"),
            Ok((
                Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(3),
                        Operand::Integer(8),
                        Operand::Integer(37)
                    ],
                },
                ""
            ))
        );
    }

    // Other unit tests: try to parse various set of instructions.

    #[test]
    fn test_empty() {
        assert_eq!(parse(""), Ok(vec![]));
    }

    #[test]
    fn test_exit() {
        // No operands.
        assert_eq!(
            parse("exit"),
            Ok(vec![Instruction {
                name: "exit".to_string(),
                operands: vec![],
            }])
        );
    }

    #[test]
    fn test_lsh() {
        // Register and immediate operands.
        assert_eq!(
            parse("lsh r3, 0x20"),
            Ok(vec![Instruction {
                name: "lsh".to_string(),
                operands: vec![Operand::Register(3), Operand::Integer(0x20)],
            }])
        );
    }

    #[test]
    fn test_ja() {
        // Jump offset operand.
        assert_eq!(
            parse("ja +1"),
            Ok(vec![Instruction {
                name: "ja".to_string(),
                operands: vec![Operand::Integer(1)],
            }])
        );
    }

    #[test]
    fn test_ldxh() {
        // Register and memory operands.
        assert_eq!(
            parse("ldxh r4, [r1+12]"),
            Ok(vec![Instruction {
                name: "ldxh".to_string(),
                operands: vec![Operand::Register(4), Operand::Memory(1, 12)],
            }])
        );
    }

    #[test]
    fn test_tcp_sack() {
        // Sample program from ubpf.
        // We could technically indent the instructions since the parser support white spaces at
        // the beginning, but there is another test for that.
        let src = "\
ldxb r2, [r1+12]
ldxb r3, [r1+13]
lsh r3, 0x8
or r3, r2
mov r0, 0x0
jne r3, 0x8, +37
ldxb r2, [r1+23]
jne r2, 0x6, +35
ldxb r2, [r1+14]
add r1, 0xe
and r2, 0xf
lsh r2, 0x2
add r1, r2
mov r0, 0x0
ldxh r4, [r1+12]
add r1, 0x14
rsh r4, 0x2
and r4, 0x3c
mov r2, r4
add r2, 0xffffffec
mov r5, 0x15
mov r3, 0x0
jgt r5, r4, +20
mov r5, r3
lsh r5, 0x20
arsh r5, 0x20
mov r4, r1
add r4, r5
ldxb r5, [r4]
jeq r5, 0x1, +4
jeq r5, 0x0, +12
mov r6, r3
jeq r5, 0x5, +9
ja +2
add r3, 0x1
mov r6, r3
ldxb r3, [r4+1]
add r3, r6
lsh r3, 0x20
arsh r3, 0x20
jsgt r2, r3, -18
ja +1
mov r0, 0x1
exit
";

        assert_eq!(
            parse(src),
            Ok(vec![
                Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 12)],
                },
                Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(3), Operand::Memory(1, 13)],
                },
                Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(8)],
                },
                Instruction {
                    name: "or".to_string(),
                    operands: vec![Operand::Register(3), Operand::Register(2)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(0), Operand::Integer(0)],
                },
                Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(3),
                        Operand::Integer(8),
                        Operand::Integer(37)
                    ],
                },
                Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 23)],
                },
                Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(2),
                        Operand::Integer(6),
                        Operand::Integer(35)
                    ],
                },
                Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 14)],
                },
                Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(1), Operand::Integer(14)],
                },
                Instruction {
                    name: "and".to_string(),
                    operands: vec![Operand::Register(2), Operand::Integer(15)],
                },
                Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(2), Operand::Integer(2)],
                },
                Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(1), Operand::Register(2)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(0), Operand::Integer(0)],
                },
                Instruction {
                    name: "ldxh".to_string(),
                    operands: vec![Operand::Register(4), Operand::Memory(1, 12)],
                },
                Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(1), Operand::Integer(20)],
                },
                Instruction {
                    name: "rsh".to_string(),
                    operands: vec![Operand::Register(4), Operand::Integer(2)],
                },
                Instruction {
                    name: "and".to_string(),
                    operands: vec![Operand::Register(4), Operand::Integer(60)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(2), Operand::Register(4)],
                },
                Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(2), Operand::Integer(4294967276)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(5), Operand::Integer(21)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(0)],
                },
                Instruction {
                    name: "jgt".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Integer(20)
                    ],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(5), Operand::Register(3)],
                },
                Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(5), Operand::Integer(32)],
                },
                Instruction {
                    name: "arsh".to_string(),
                    operands: vec![Operand::Register(5), Operand::Integer(32)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(4), Operand::Register(1)],
                },
                Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(4), Operand::Register(5)],
                },
                Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(5), Operand::Memory(4, 0)],
                },
                Instruction {
                    name: "jeq".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Integer(1),
                        Operand::Integer(4)
                    ],
                },
                Instruction {
                    name: "jeq".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Integer(0),
                        Operand::Integer(12)
                    ],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(6), Operand::Register(3)],
                },
                Instruction {
                    name: "jeq".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Integer(5),
                        Operand::Integer(9)
                    ],
                },
                Instruction {
                    name: "ja".to_string(),
                    operands: vec![Operand::Integer(2)],
                },
                Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(1)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(6), Operand::Register(3)],
                },
                Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(3), Operand::Memory(4, 1)],
                },
                Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(3), Operand::Register(6)],
                },
                Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(32)],
                },
                Instruction {
                    name: "arsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(32)],
                },
                Instruction {
                    name: "jsgt".to_string(),
                    operands: vec![
                        Operand::Register(2),
                        Operand::Register(3),
                        Operand::Integer(-18)
                    ],
                },
                Instruction {
                    name: "ja".to_string(),
                    operands: vec![Operand::Integer(1)],
                },
                Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(0), Operand::Integer(1)],
                },
                Instruction {
                    name: "exit".to_string(),
                    operands: vec![],
                }
            ])
        );
    }

    /// When running without `std` the `EasyParser` provided by `combine`
    /// cannot be used. Because of this we need to use the `Parser` and the
    /// error messages are different.
    #[test]
    fn test_error_eof() {
        let expected_error;
        #[cfg(feature = "std")]
        {
            expected_error = Err(
                "Parse error at line: 1, column: 6\nUnexpected end of input\nExpected digit\n"
                    .to_string(),
            );
        }
        #[cfg(not(feature = "std"))]
        {
            expected_error = Err("unexpected parse".to_string());
        }
        // Unexpected end of input in a register name.
        assert_eq!(parse("lsh r"), expected_error);
    }

    /// When running without `std` the `EasyParser` provided by `combine`
    /// cannot be used. Because of this we need to use the `Parser` and the
    /// error messages are different.
    #[test]
    fn test_error_unexpected_character() {
        let expected_error;
        #[cfg(feature = "std")]
        {
            expected_error = Err(
                "Parse error at line: 2, column: 1\nUnexpected `^`\nExpected letter or digit, whitespaces, `r`, `-`, `+`, `[` or end of input\n".to_string()
            );
        }
        #[cfg(not(feature = "std"))]
        {
            expected_error = Err("unexpected parse".to_string());
        }
        // Unexpected character at end of input.
        assert_eq!(parse("exit\n^"), expected_error);
    }

    #[test]
    fn test_initial_whitespace() {
        assert_eq!(
            parse(
                " 
                          exit"
            ),
            Ok(vec![Instruction {
                name: "exit".to_string(),
                operands: vec![],
            }])
        );
    }
}
