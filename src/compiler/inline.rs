use num_bigint::ToBigInt;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::rc::Rc;

use clvm_rs::allocator::Allocator;

use crate::classic::clvm::__type_compatibility__::bi_one;
use crate::classic::clvm_tools::stages::stage_0::TRunProgram;

use crate::compiler::codegen::{
    generate_expr_code,
    get_callable,
    get_call_name
};
use crate::compiler::comptypes::{
    BodyForm, CompileErr, CompiledCode, CompilerOpts, InlineFunction, PrimaryCodegen, Callable
};
use crate::compiler::sexp::{decode_string, SExp};
use crate::compiler::srcloc::Srcloc;

use crate::util::{u8_from_number, Number};

fn apply_fn(loc: Srcloc, name: String, expr: Rc<BodyForm>) -> Rc<BodyForm> {
    Rc::new(BodyForm::Call(
        loc.clone(),
        vec![
            Rc::new(BodyForm::Value(SExp::atom_from_string(
                loc.clone(),
                &"@".to_string(),
            ))),
            expr
        ],
    ))
}

fn at_form(loc: Srcloc, path: Number) -> Rc<BodyForm> {
    apply_fn(
        loc.clone(),
        "@".to_string(),
        Rc::new(BodyForm::Value(SExp::Integer(loc.clone(), path.clone())))
    )
}

pub fn synthesize_args(arg_: Rc<SExp>) -> Vec<Rc<BodyForm>> {
    let mut start = 5_i32.to_bigint().unwrap();
    let mut result = Vec::new();
    let mut arg = arg_.clone();
    loop {
        match arg.borrow() {
            SExp::Cons(l, _, b) => {
                result.push(at_form(l.clone(), start.clone()));
                start = bi_one() + start.clone() * 2_i32.to_bigint().unwrap();
                arg = b.clone();
            }
            _ => {
                return result;
            }
        }
    }
}

fn enlist_remaining_args(loc: Srcloc, arg_choice: usize, args: &Vec<Rc<BodyForm>>) -> Rc<BodyForm> {
    let mut result_body = BodyForm::Value(SExp::Nil(loc.clone()));

    for i_reverse in arg_choice..args.len() {
        let i = args.len() - i_reverse - 1;
        result_body = BodyForm::Call(
            loc.clone(),
            vec!(
                Rc::new(BodyForm::Value(SExp::atom_from_string(loc.clone(), &"c".to_string()))),
                args[i].clone(),
                Rc::new(result_body)
            )
        );
    }

    Rc::new(result_body)
}

fn pick_value_from_arg_element(match_args: Rc<SExp>, provided: Rc<BodyForm>, apply: &dyn Fn(Rc<BodyForm>) -> Rc<BodyForm>, name: Vec<u8>) -> Option<Rc<BodyForm>> {
    match match_args.borrow() {
        SExp::Cons(l, a, b) => {
            let matched_a = pick_value_from_arg_element(a.clone(), provided.clone(), &|x| {
                apply_fn(l.clone(), "f".to_string(), x.clone())
            }, name.clone());
            let matched_b = pick_value_from_arg_element(b.clone(), provided.clone(), &|x| {
                apply_fn(l.clone(), "r".to_string(), x.clone())
            }, name.clone());

            match (matched_a, matched_b) {
                (Some(a), _) => Some(a),
                (_, Some(b)) => Some(b),
                _ => None
            }
        },
        SExp::Atom(_, a) => {
            if *a == name {
                Some(provided)
            } else {
                None
            }
        }
        _ => None
    }
}

fn arg_lookup(match_args: Rc<SExp>, arg_choice: usize, args: &Vec<Rc<BodyForm>>, name: Vec<u8>) -> Option<Rc<BodyForm>> {
    match match_args.borrow() {
        SExp::Cons(l, f, r) => {
            match pick_value_from_arg_element(f.clone(), args[arg_choice].clone(), &|x| x.clone(), name.clone()) {
                Some(x) => Some(x),
                None => arg_lookup(r.clone(), arg_choice + 1, args, name.clone())
            }
        },
        _ => pick_value_from_arg_element(match_args.clone(), enlist_remaining_args(match_args.loc(), arg_choice, args), &|x: Rc<BodyForm>| x.clone(), name)
    }
}

fn get_inline_callable(
    opts: Rc<dyn CompilerOpts>,
    compiler: &PrimaryCodegen,
    loc: Srcloc,
    list_head: Rc<BodyForm>
) -> Result<Callable, CompileErr> {
    let list_head_borrowed: &BodyForm = list_head.borrow();
    let name = get_call_name(loc.clone(), list_head_borrowed.clone())?;
    get_callable(opts, compiler, loc.clone(), name.clone())
}

fn replace_inline_body(
    allocator: &mut Allocator,
    runner: Rc<dyn TRunProgram>,
    opts: Rc<dyn CompilerOpts>,
    compiler: &PrimaryCodegen,
    loc: Srcloc,
    inline: &InlineFunction,
    args: &Vec<Rc<BodyForm>>,
    expr: Rc<BodyForm>,
) -> Result<Rc<BodyForm>, CompileErr> {
    let arg_str_vec: Vec<String> = args.iter().map(|x| x.to_sexp().to_string()).collect();

    println!("replace_inline_body {} function {} expr {} args {:?}", SExp::Atom(loc.clone(), inline.name.to_vec()).to_string(), inline.to_sexp().to_string(), expr.to_sexp().to_string(), arg_str_vec);

    match expr.borrow() {
        BodyForm::Let(l, _, bindings, body) => Err(CompileErr(
            loc.clone(),
            "let binding should have been hoisted before optimization".to_string(),
        )),
        BodyForm::Call(l, call_args) => {
            let mut new_args = Vec::new();
            for i in 0..call_args.len() {
                if i == 0 {
                    new_args.push(call_args[i].clone());
                } else {
                    let replaced =
                        replace_inline_body(
                            allocator,
                            runner.clone(),
                            opts.clone(),
                            compiler,
                            call_args[i].loc(),
                            inline,
                            &args.clone(),
                            call_args[i].clone()
                        )?;
                    new_args.push(replaced);
                }
            }
            // If the called function is an inline, we'll expand it here.
            // This is so we can preserve the context of argument expressions
            // so no new mapping is needed.  This solves a number of problems
            // with the previous implementation.
            //
            // If it's a macro we'll expand it here so we can recurse and
            // determine whether an inline is the next level.
            match get_inline_callable(opts.clone(), compiler, l.clone(), call_args[0].clone())? {
                Callable::CallInline(_, new_inline) => {
                    replace_in_inline(
                        allocator,
                        runner,
                        opts.clone(),
                        compiler,
                        l.clone(),
                        &new_inline,
                        &new_args
                    )
                },
                Callable::CallMacro(_, macro_body) => {
                    panic!("expand macro and reprocess");
                },
                _ => {
                    Ok(Rc::new(BodyForm::Call(l.clone(), new_args)))
                }
            }
        },
        BodyForm::Value(SExp::Atom(_, a)) => arg_lookup(inline.args.clone(), 0, args, a.clone())
            .map(|x| Ok(x.clone()))
            .unwrap_or_else(|| Ok(expr.clone())),
        _ => Ok(expr.clone())
    }
}

pub fn replace_in_inline(
    allocator: &mut Allocator,
    runner: Rc<dyn TRunProgram>,
    opts: Rc<dyn CompilerOpts>,
    compiler: &PrimaryCodegen,
    loc: Srcloc,
    inline: &InlineFunction,
    args: &Vec<Rc<BodyForm>>,
) -> Result<Rc<BodyForm>, CompileErr> {
    let arg_str_vec: Vec<String> = args.iter().map(|x| x.to_sexp().to_string()).collect();
    let res = replace_inline_body(
        allocator,
        runner,
        opts,
        compiler,
        loc.clone(),
        inline,
        args,
        inline.body.clone(),
    )?;
    println!("replace_in_inline (defun-inline {} {} {}) with args {:?} gives {}", SExp::Atom(loc.clone(), inline.name.clone()).to_string(), inline.args.to_string(), inline.to_sexp().to_string(), arg_str_vec, res.to_sexp().to_string());
    Ok(res)
}
