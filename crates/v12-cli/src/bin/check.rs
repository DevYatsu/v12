use v12_bccompiler::{compile_source_with_interner, freeze_interner, Interner};
use v12_bytecode::{Const, Opcode};

fn main() {
    let src = "(x => console.log(x))(2)";
    let mut interner = Interner::default();
    let prog = compile_source_with_interner(src, &mut interner).unwrap();
    let resolver = freeze_interner(interner);
    let strings: Vec<String> = resolver.iter().map(|(_, s)| s.to_string()).collect();
    println!("strings vec: {:?}", strings);
    let arrow = &prog.functions[1];
    println!("arrow consts: {:?}", arrow.consts.iter().collect::<Vec<_>>());
    for (pc, instr) in arrow.instrs.iter().enumerate() {
        println!(
            "pc {}: {:?} a={} b={} c={} imm16={}",
            pc,
            instr.op(),
            instr.a(),
            instr.b(),
            instr.c(),
            instr.imm16()
        );
        if instr.op() == Some(Opcode::GetGlobal) {
            let str_id = instr.imm16() as u32;
            let text = strings
                .get(str_id as usize)
                .map(|s| s.as_str())
                .unwrap_or("<missing>");
            println!("  GetGlobal str_id {} => {:?}", str_id, text);
        }
        if instr.op() == Some(Opcode::LoadConst) {
            let pool_idx = instr.imm16() as u16;
            if let Some(c) = arrow.consts.get(pool_idx) {
                println!("  LoadConst pool_idx {} => {:?}", pool_idx, c);
                if let Const::Str32(sid) = c {
                    let text = strings
                        .get(sid as usize)
                        .map(|s| s.as_str())
                        .unwrap_or("<missing>");
                    println!("    Str32 sid {} => {:?}", sid, text);
                }
            }
        }
        let _ = strings;
        if instr.op() == Some(Opcode::GetProperty) {
            println!(
                "  GetProperty dst r{} obj r{} key r{}",
                instr.a(),
                instr.b(),
                instr.c()
            );
        }
    }
    let get_global_str_id = arrow
        .instrs
        .iter()
        .find(|i| i.op() == Some(Opcode::GetGlobal))
        .map(|i| i.imm16() as u32)
        .unwrap();
    let load_const_pool_idx = arrow
        .instrs
        .iter()
        .find(|i| i.op() == Some(Opcode::LoadConst))
        .map(|i| i.imm16() as u16)
        .unwrap();
    let load_const_str_id = match arrow.consts.get(load_const_pool_idx).unwrap() {
        Const::Str32(id) => id,
        _ => panic!("not str"),
    };
    println!(
        "GetGlobal str_id={}, LoadConst Str32 payload={}",
        get_global_str_id, load_const_str_id
    );
    if get_global_str_id == load_const_str_id {
        println!("BUG: same string id for console and log!");
    } else {
        println!(
            "OK: distinct string ids (console={}, log={})",
            get_global_str_id, load_const_str_id
        );
    }
}
