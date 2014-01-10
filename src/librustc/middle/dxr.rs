// TODO licence

// output data for analysis by the dxr rust plugin

use driver::driver::CrateAnalysis;
use driver::session::Session;
use middle::ty;
use middle::typeck;
use middle::typeck::lookup_def_tcx;

use syntax::ast;
use syntax::ast::*;
use syntax::ast_map::*;
use syntax::codemap::*;
use syntax::diagnostic;
use syntax::parse::lexer;
use syntax::parse::lexer::{reader, StringReader};
use syntax::parse::token::{get_ident_interner,ident_to_str,is_keyword,keywords,to_str,is_ident,Token,EOF,EQ,COLON};
use syntax::visit;
use syntax::visit::Visitor;
use syntax::print::pprust::path_to_str;

use std::io;
use std::io::File;
use std::io::fs;
use std::os;
use std::path::Path;


struct DxrVisitor<'l> {
    sess: Session,
    analysis: &'l CrateAnalysis,

    collected_paths: ~[(NodeId, ast::Path)],

    // output file
    out: ~Writer,
}

impl <'l> DxrVisitor<'l> {
    // standard string for extents/location
    // TODO check file name includes path
    // sub_span starts at span.lo, so we need to adjust the positions etc.
    // if sub_span is None, we don't need to adjust.
    fn extent_str(&self, span:&Span, sub_span: Option<&Span>) -> ~str {
        let cm = self.sess.codemap;
        let base_loc = cm.lookup_char_pos(span.lo);
        let base_pos: CharPos = cm.bytepos_to_local_charpos(BytePos(*span.lo - *base_loc.file.start_pos));

        let mut lo_loc = base_loc;
        let mut hi_loc = base_loc;
        let mut lo_pos: uint;
        let mut hi_pos: uint;

        match sub_span {
            Some(ss) => {
                let sub_lo = cm.lookup_char_pos(ss.lo);
                let sub_hi = cm.lookup_char_pos(ss.hi);
                lo_loc.line = base_loc.line + sub_lo.line - 1;
                lo_loc.col = CharPos(*base_loc.col + *sub_lo.col);
                hi_loc.line = base_loc.line + sub_hi.line - 1;
                hi_loc.col = CharPos(*base_loc.col + *sub_hi.col);
                lo_pos = *base_pos + *cm.bytepos_to_local_charpos(BytePos(*ss.lo - *sub_lo.file.start_pos));
                hi_pos = *base_pos + *cm.bytepos_to_local_charpos(BytePos(*ss.hi - *sub_hi.file.start_pos));
            },
            None => {
                hi_loc = cm.lookup_char_pos(span.hi);
                lo_pos = *base_pos;
                let cph: CharPos = cm.bytepos_to_local_charpos(BytePos(*span.hi - *hi_loc.file.start_pos));
                hi_pos = *cph;
            }
        }

        return format!("file_name,{},file_line,{},file_col,{},extent_start,{},file_line_end,{},file_col_end,{},extent_end,{}",
                       lo_loc.file.name, lo_loc.line, *lo_loc.col, lo_pos,
                       hi_loc.line, *hi_loc.col, hi_pos);
    }

    fn retokenise_span(&self, span: &Span) -> @mut StringReader {
        // sadness - we don't have spans for sub-expressions nor access to the tokens
        // so in order to get extents for the function name itself (which dxr expects)
        // we need to re-tokenise the fn definition
        let cm = self.sess.codemap;
        let handler = diagnostic::mk_handler(None);
        let handler = diagnostic::mk_span_handler(handler, cm);

        let src_str = match cm.span_to_snippet(*span) {
            Some(s) => s,
            None => ~"",
        };
        let filemap = cm.new_filemap(@"<anon>",
                                     src_str.to_managed());
        lexer::new_string_reader(handler, filemap)
    }

    // Re-parses a path and returns the span for the last identifier in the path
    fn span_for_name(&self, span: &Span) -> Span {
        // If we can't find a name to select, select the entire expression. This might
        // screw things up later in DXR because we might overlap with a sub-expression.
        // But at least DXR will get all hissy then.
        let mut result = *span;

        let toks = self.retokenise_span(span);
        loop {
            let ts = toks.next_token();
            if ts.tok == EOF {
                return result
            }
            if is_ident(&ts.tok) {
                result = ts.sp;
            }
        }
    }

    fn sub_span_before_token(&self, span: &Span, tok: Token) -> Option<Span> {
        let toks = self.retokenise_span(span);
        let mut prev = toks.next_token();
        loop {
            if prev.tok == EOF {
                return None;
            }
            let next = toks.next_token();
            if next.tok == tok {
                return Some(prev.sp);
            }
            prev = next;
        }
    }

    fn sub_span_after_keyword(&self, span: &Span, keyword: keywords::Keyword) -> Option<Span> {
        let toks = self.retokenise_span(span);
        loop {
            let ts = toks.next_token();
            if ts.tok == EOF {
                return None;
            }
            if is_keyword(keyword, &ts.tok) {
                let ts = toks.next_token();
                return Some(ts.sp);
            }
        }
    }

    fn spans_for_path_segments(&self, path: &ast::Path) -> ~[Span] {
        let mut result: ~[Span] = ~[];

        let toks = self.retokenise_span(&path.span);
        loop {
            let ts = toks.next_token();
            if ts.tok == EOF {
                return result
            }
            if is_ident(&ts.tok) {
                //TODO only push the spans for segment idents, not type params
                result.push(ts.sp);
            }
        }
    }

    fn process_path_prefixes(&self, path: &ast::Path) -> ~[(Span, ~str)] {
        let spans = self.spans_for_path_segments(path);

        if spans.len() != path.segments.len() {
            println!("Miscalculated spans for path '{}'. Found {} spans, expected {}",
                     path_to_str(path, get_ident_interner()), spans.len(), path.segments.len());
            return ~[];
        }

        let mut result = ~[];
        for i in range(0, path.segments.len()) {
            let mut segs = path.segments.to_owned();
            segs.truncate(i+1);
            let sub_path = ast::Path{span: spans[i],
                                     global: path.global,
                                     segments: segs};

            let qualname = path_to_str(&sub_path, get_ident_interner());
            result.push((spans[i], qualname));
        }
        result
    }

    fn variable_str(&self, span: &Span, sub_span: &Span, id: NodeId, name: &str) -> ~str {
        format!("variable,{},id,{},name,{},qualname,{}\n",
                self.extent_str(span, Some(sub_span)), id, name,
                name + "$" + id.to_str())
    }

    fn field_str(&self, span: &Span, sub_span: &Span, id: NodeId, name: &str, qualname: &str) -> ~str {
        format!("variable,{},id,{},name,{},qualname,{}\n",
                self.extent_str(span, Some(sub_span)), id, name, qualname)
    }

    fn fn_str(&self, span: &Span, sub_span: &Span, id: NodeId, name: &str) -> ~str {
        format!("function,{},qualname,{},id,{}\n",
                self.extent_str(span, Some(sub_span)), name, id)
    }

    fn struct_str(&self, span: &Span, sub_span: &Span, id: NodeId, ctor_id: NodeId, name: &str) -> ~str {
        format!("struct,{},id,{},ctor_id,{},qualname,{}\n",
                self.extent_str(span, Some(sub_span)), id, ctor_id, name)
    }

    fn trait_str(&self, span: &Span, sub_span: &Span, id: NodeId, name: &str) -> ~str {
        format!("trait,{},id,{},qualname,{}\n",
                self.extent_str(span, Some(sub_span)), id, name)
    }

    fn impl_str(&self, span: &Span, sub_span: &Span, id: NodeId, ref_id: NodeId) -> ~str {
        format!("impl,{},id,{},refid,{}\n",
                self.extent_str(span, Some(sub_span)), id, ref_id)        
    }

    fn mod_str(&self, span: &Span, sub_span: &Span, id: NodeId, name: &str, parent: NodeId) -> ~str {
        format!("module,{},id,{},qualname,{},parent,{}\n",
                self.extent_str(span, Some(sub_span)), id, name, parent)
    }

    fn mod_alias_str(&self, span: &Span, sub_span: &Span, id: NodeId, mod_id: NodeId, name: &str) -> ~str {
        format!("module_alias,{},id,{},refid,{},name,{}\n",
                self.extent_str(span, Some(sub_span)), id, mod_id, name)
    }

    fn ref_str(&self, kind: &str, span: &Span, sub_span: &Span, id: NodeId) -> ~str {
        format!("{},{},refid,{}\n",
                kind, self.extent_str(span, Some(sub_span)), id)
    }

    fn mod_ref_str(&self, span: &Span, sub_span: Option<&Span>, id: NodeId) -> ~str {
        format!("mod_ref,{},refid,{},qualname,\"\"\n",
                self.extent_str(span, sub_span), id)
    }

    fn sub_mod_ref_str(&self, span: &Span, sub_span: &Span, qualname: &str) -> ~str {
        format!("mod_ref,{},refid,0,qualname,{}\n",
                self.extent_str(span, Some(sub_span)), qualname)
    }

    fn inherit_str(&self, base_id: NodeId, deriv_id: NodeId) -> ~str {
        format!("inheritance,base,{},derived,{}\n",
                base_id, deriv_id)        
    }
}

impl<'l> Visitor<DxrVisitorEnv> for DxrVisitor<'l> {
    fn visit_item(&mut self, item:@item, e: DxrVisitorEnv) {
        match item.node {
            item_fn(decl, _, _, _, body) => {
                let path = match *self.analysis.ty_cx.items.get(&item.id) {
                    node_item(_, path) => path_ident_to_str(path, item.ident, get_ident_interner()),
                    _ => ~""
                };


                match self.sub_span_after_keyword(&item.span, keywords::Fn) {
                    Some(sub_span) => write!(self.out, "{}",
                                             self.fn_str(&item.span,
                                                         &sub_span,
                                                         item.id,
                                                         path)),
                    None => println("Could not find sub-span for fn name"),
                }

                for arg in decl.inputs.iter() {
                    self.visit_pat(arg.pat, e);
                    for &(id, ref p) in self.collected_paths.iter() {
                        // get the span only for the name of the variable (I hope the path is only ever a
                        // variable name, but who knows?)
                        let sub_span = self.span_for_name(&p.span);
                        write!(self.out, "{}",
                               self.variable_str(&p.span,
                                                 &sub_span,
                                                 id,
                                                 path_to_str(p, get_ident_interner())));
                    }
                    self.collected_paths.clear();
                }

                // walk arg and return types
                for arg in decl.inputs.iter() {
                    self.visit_ty(arg.ty, e);
                }
                self.visit_ty(decl.output, e);

                // walk the body
                self.visit_block(body, e);

                // TODO walk type params
            },
            item_static(typ, _, expr) => {
                match self.sub_span_after_keyword(&item.span, keywords::Static) {
                    // XXX getting a fully qualified name for a variable is hard because in
                    // the local case they can be overridden in one block and there is no nice
                    // way to refer to a scope in english, so we just hack it by appending the
                    // variable def's node id
                    Some(sub_span) => write!(self.out, "{}",
                                             self.variable_str(&item.span,
                                                               &sub_span,
                                                               item.id,
                                                               ident_to_str(&item.ident))),
                    None => println("Could not find sub-span for static item name"),
                }

                // walk type and init value
                self.visit_ty(typ, e);
                self.visit_expr(expr, e);
            },
            item_struct(def, ref g) => {
                let qualname = match *self.analysis.ty_cx.items.get(&item.id) {
                    node_item(_, path) => path_ident_to_str(path, item.ident, get_ident_interner()),
                    _ => ~""
                };

                let ctor_id = match def.ctor_id {
                    Some(node_id) => node_id,
                    None => 0,
                };
                match self.sub_span_after_keyword(&item.span, keywords::Struct) {
                    Some(sub_span) => write!(self.out, "{}",
                                             self.struct_str(&item.span,
                                                             &sub_span,
                                                             item.id, ctor_id,
                                                             qualname)),
                    None => println!("Could not find sub-span for struct {}", qualname),
                }

                // fields
                for field in def.fields.iter() {
                    match field.node.kind {
                        named_field(ref ident, _) => {
                            let name = ident_to_str(ident);
                            let qualname = qualname + "::" + name;
                            match self.sub_span_before_token(&field.span, COLON) {
                                Some(ref sub_span) => write!(self.out, "{}",
                                                             self.field_str(&field.span,
                                                                            sub_span,
                                                                            field.node.id,
                                                                            name,
                                                                            qualname)),
                                None => println!("Could not find sub-span for field {}", qualname),
                            }
                        },
                        _ => (),
                    }
                }

                // TODO walk type params
            },
            item_impl(ref type_parameters,
                      ref trait_ref,
                      typ,
                      ref methods) => {
                match typ.node {
                    ty_path(ref path, ref bounds, id) => {
                        let def_map = self.analysis.ty_cx.def_map.borrow();
                        let def = def_map.get().find(&id);
                        match def {
                            Some(d) => match *d {
                                ast::DefTy(def_id) =>
                                    if def_id.crate == 0 {
                                        let sub_span = self.span_for_name(&path.span);
                                        write!(self.out, "{}",
                                               self.impl_str(&path.span, &sub_span, item.id, def_id.node));
                                    },
                                _ => println!("found something else in impl.type {}", path_to_str(path, get_ident_interner())),
                            },
                            _ => println("failed to look up the def of the struct in an impl"),
                        }
                    },
                    _ => println("expected a path to a struct, but got some other type"),
                }

                match *trait_ref {
                    Some(ref trait_ref) => {
                        // TODO factor all this out as a method
                        let def_map = self.analysis.ty_cx.def_map.borrow();
                        let def = def_map.get().find(&trait_ref.ref_id);
                        match def {
                            Some(d) => match *d {
                                ast::DefTrait(def_id) => if def_id.crate == 0 {
                                    let sub_span = self.span_for_name(&trait_ref.path.span);
                                    write!(self.out, "{}",
                                           self.ref_str("type_ref", &trait_ref.path.span, &sub_span, def_id.node));
                                    write!(self.out, "{}",
                                           self.impl_str(&trait_ref.path.span, &sub_span, item.id, def_id.node));
                                },
                                _ => println("found something else in trait lookup"),
                            },
                            None => println!("could not find trait def {}", trait_ref.ref_id),
                        };
                    },
                    None => (),
                }

                self.visit_generics(type_parameters, e);
                self.visit_ty(typ, e);
                for method in methods.iter() {
                    visit::walk_method_helper(self, *method, e)
                }
            },
            item_trait(ref generics, ref trait_refs, ref methods) => {
                let qualname = match *self.analysis.ty_cx.items.get(&item.id) {
                    node_item(_, path) => path_ident_to_str(path, item.ident, get_ident_interner()),
                    _ => ~""
                };

                match self.sub_span_after_keyword(&item.span, keywords::Trait) {
                    Some(sub_span) => write!(self.out, "{}",
                                             self.trait_str(&item.span,
                                                            &sub_span,
                                                            item.id,
                                                            qualname)),
                    None => println!("Could not find sub-span for trait {}", qualname),
                }

                // super-traits
                let def_map = self.analysis.ty_cx.def_map.borrow();
                for trait_ref in trait_refs.iter() {
                    let def = def_map.get().find(&trait_ref.ref_id);
                    match def {
                        Some(d) => match *d {
                            ast::DefTrait(def_id) => if def_id.crate == 0 {
                                let sub_span = self.span_for_name(&trait_ref.path.span);
                                write!(self.out, "{}",
                                       self.ref_str("type_ref", &trait_ref.path.span, &sub_span, def_id.node));
                                write!(self.out, "{}",
                                       self.inherit_str(def_id.node, item.id));
                            },
                            _ => println("found something else in trait lookup"),
                        },
                        None => println!("could not find trait def {}", trait_ref.ref_id),
                    };

                }

                // walk generics and methods
                self.visit_generics(generics, e);
                for method in methods.iter() {
                    self.visit_trait_method(method, e)
                }
            },
            item_mod(ref m) => {
                let qualname = match *self.analysis.ty_cx.items.get(&item.id) {
                    node_item(_, path) => path_ident_to_str(path, item.ident, get_ident_interner()),
                    _ => ~""
                };
                match self.sub_span_after_keyword(&item.span, keywords::Mod) {
                    Some(sub_span) => write!(self.out, "{}",
                                             self.mod_str(&item.span,
                                                          &sub_span,
                                                          item.id,
                                                          qualname,
                                                          e.cur_mod)),
                    None => println!("Could not find sub-span for module {}", qualname),
                }

                visit::walk_mod(self, m, DxrVisitorEnv::new_nested(item.id))
            },
            _ => visit::walk_item(self, item, e),
        }
    }

    fn visit_view_item(&mut self, i:&view_item, e:DxrVisitorEnv) {
        match i.node {
            view_item_use(ref paths) => {
                let def_map = self.analysis.ty_cx.def_map.borrow();
                for vp in paths.iter() {
                    match vp.node {
                        view_path_simple(ident, ref path, id) => {
                            let sub_span = self.span_for_name(&vp.span);
                            let def = def_map.get().find(&id);
                            let mut mod_id = 0;
                            match def {
                                Some(d) => match *d {
                                    ast::DefMod(def_id) => if def_id.crate == 0 {
                                        mod_id = def_id.node;
                                        write!(self.out, "{}",
                                               self.mod_ref_str(&vp.span, Some(&sub_span), mod_id));                                        
                                    },
                                    _ => println("Looking up a module, but did not find a DefMod"),
                                },
                                None => println!("Could not find module definition for {}", id),
                            }

                            // 'use' always introduces a module alias, if there is not an explicit
                            // one, there is an implicit one.
                            let sub_span = match self.sub_span_before_token(&vp.span, EQ) {
                                Some(sub_span) => sub_span,
                                None => sub_span,
                            };

                            let name = ident_to_str(&ident);
                            write!(self.out, "{}",
                                   self.mod_alias_str(&vp.span,
                                                      &sub_span,
                                                      id, mod_id,
                                                      name));
                            let mut sub_paths = self.process_path_prefixes(path);
                            // We've already process the entire path above, so let's not do it
                            // again.
                            let len = sub_paths.len();
                            sub_paths.truncate(len-1);
                            for &(ref span, ref qualname) in sub_paths.iter() {
                                write!(self.out, "{}",
                                       self.sub_mod_ref_str(&path.span, span, *qualname));                                                                        
                            }
                        }
                        view_path_glob(ref path, _) => {
                            let sub_paths = self.process_path_prefixes(path);
                            for &(ref span, ref qualname) in sub_paths.iter() {
                                write!(self.out, "{}",
                                       self.sub_mod_ref_str(&path.span, span, *qualname));                                                                        
                            }
                        }
                        view_path_list(ref path, ref list, _) => {
                            for id in list.iter() {
                                let def = def_map.get().find(&id.node.id);
                                match def {
                                    Some(d) => match *d {
                                        ast::DefMod(def_id) => if def_id.crate == 0 {
                                            write!(self.out, "{}",
                                                   self.mod_ref_str(&id.span, None, def_id.node));
                                        },
                                        _ => println("Looking up a module, but did not find a DefMod"),
                                    },
                                    None => println!("Could not find module definition for {}", id.node.id),
                                }
                            }

                            let sub_paths = self.process_path_prefixes(path);
                            for &(ref span, ref qualname) in sub_paths.iter() {
                                write!(self.out, "{}",
                                       self.sub_mod_ref_str(&path.span, span, *qualname));                                                                        
                            }
                        }
                    }
                }
            },
            _ => visit::walk_view_item(self, i, e),
        }
    }

    fn visit_ty(&mut self, t:&Ty, e:DxrVisitorEnv) {
        match t.node {
            ty_path(ref path, ref bounds, id) => {
                let def_map = self.analysis.ty_cx.def_map.borrow();
                let def = def_map.get().find(&id);
                match def {
                    Some(d) => match *d {
                        ast::DefTy(def_id) |
                        ast::DefTrait(def_id) => if def_id.crate == 0 {
                            let sub_span = self.span_for_name(&t.span);
                            write!(self.out, "{}",
                                   self.ref_str("type_ref", &t.span, &sub_span, def_id.node));
                        },
                        _ => println!("found something else in type {}", path_to_str(path, get_ident_interner())),
                    },
                    _ => (),
                }

                let mut sub_paths = self.process_path_prefixes(path);
                // We've already process the entire path above, so let's not do it
                // again.
                let len = sub_paths.len();
                sub_paths.truncate(len-1);
                for &(ref span, ref qualname) in sub_paths.iter() {
                    write!(self.out, "{}",
                           self.sub_mod_ref_str(&path.span, span, *qualname));                                                                        
                }

                visit::walk_path(self, path, e);
                for bounds in bounds.iter() {
                    visit::walk_ty_param_bounds(self, bounds, e)
                }
            },
            _ => visit::walk_ty(self, t, e),
        }
    }

    fn visit_expr(&mut self, ex: @Expr, e: DxrVisitorEnv) {
        match ex.node {
            ExprCall(f, ref args, _) => {
                let def_map = self.analysis.ty_cx.def_map.borrow();
                let def = def_map.get().find(&f.id);
                let sub_span = self.span_for_name(&f.span);
                match def {
                    Some(d) => match *d {
                        ast::DefFn(id, _) => if id.crate == 0 {
                            write!(self.out, "{}",
                                   self.ref_str("fn_call", &f.span, &sub_span, id.node));
                        },
                        ast::DefLocal(id, _) => {
                            // we are losing the information that we have a function call, not
                            // just a plain variable reference, not sure if that matters.
                            write!(self.out, "{}",
                                   self.ref_str("var_ref", &f.span, &sub_span, id));
                        },
                        ast::DefStatic(id,_) => if id.crate == 0 {
                            write!(self.out, "{}",
                                   self.ref_str("var_ref", &f.span, &sub_span, id.node));
                        },
                        // TODO fields
                        _ => println("Looking up a function call, but did not find a DefFn"),
                    },
                    None => println!("Could not find function definition {}", f.id),
                }

                for arg in args.iter() {
                    self.visit_expr(*arg, e);
                }
            },
            ExprPath(ref path) => {
                let def_map = self.analysis.ty_cx.def_map.borrow();
                let def = def_map.get().find(&ex.id);
                let sub_span = self.span_for_name(&ex.span);
                match def {
                    Some(d) => match *d {
                        ast::DefLocal(id, _) |
                        ast::DefArg(id, _) => {
                            write!(self.out, "{}",
                                   self.ref_str("var_ref", &ex.span, &sub_span, id));
                        },
                        ast::DefStatic(def_id,_) => if def_id.crate == 0 {
                            write!(self.out, "{}",
                                   self.ref_str("var_ref", &ex.span, &sub_span, def_id.node));
                        },
                        ast::DefStruct(def_id) => if def_id.crate == 0 {
                            write!(self.out, "{}",
                                   self.ref_str("struct_ref", &ex.span, &sub_span, def_id.node));
                        },
                        ast::DefFn(def_id, _) => if def_id.crate == 0 {
                            write!(self.out, "{}",
                                   self.ref_str("fn_call", &ex.span, &sub_span, def_id.node));
                        },
                       _ => println!("Unexpected def kind while looking up path {}", ex.id),
                    },
                    None => println!("Could not find path {}", ex.id),
                }

                let mut sub_paths = self.process_path_prefixes(path);
                // We've already processed the entire path above, so let's not do it
                // again.
                let len = sub_paths.len();
                sub_paths.truncate(len-1);
                for &(ref span, ref qualname) in sub_paths.iter() {
                    write!(self.out, "{}",
                           self.sub_mod_ref_str(&path.span, span, *qualname));                                                                        
                }

                visit::walk_path(self, path, e);
            },
            ExprStruct(ref path, ref fields, base) => {
                let def_map = self.analysis.ty_cx.def_map.borrow();
                let def = def_map.get().find(&ex.id);
                let mut struct_def: Option<DefId> = None;
                match def {
                    Some(d) => match *d {
                        ast::DefStruct(def_id) => if def_id.crate == 0 {
                            struct_def = Some(def_id);
                            let sub_span = self.span_for_name(&path.span);
                            write!(self.out, "{}",
                                   self.ref_str("struct_ref", &path.span, &sub_span, def_id.node));
                        },
                        _ => println("Expected a struct, but did not find a DefStruct"),
                    },
                    None => println!("Could not find struct {}", ex.id),
                }

                // TODO used in three places now, (plus two without the truncate) - refactor it!
                let mut sub_paths = self.process_path_prefixes(path);
                // We've already processed the entire path above, so let's not do it
                // again.
                let len = sub_paths.len();
                sub_paths.truncate(len-1);
                for &(ref span, ref qualname) in sub_paths.iter() {
                    write!(self.out, "{}",
                           self.sub_mod_ref_str(&path.span, span, *qualname));                                                                        
                }

                for field in fields.iter() {
                    match struct_def {
                        Some(struct_def) => {
                            let fields = ty::lookup_struct_fields(self.analysis.ty_cx, struct_def);
                            for f in fields.iter() {
                                if f.name == field.ident.node.name {
                                    // TODO don't need a sub-span
                                    let sub_span = self.span_for_name(&field.ident.span);
                                    write!(self.out, "{}",
                                           self.ref_str("var_ref", &field.ident.span, &sub_span, f.id.node));
                                }
                            }
                        },
                        _ => (),
                    }

                    self.visit_expr(field.expr, e)
                }
                visit::walk_expr_opt(self, base, e)
            },
            // TODO - methods
            /*ExprMethodCall(_, _, _, _, _, _) => {
                if (!self.analysis.maps.method_map.contains_key(&ex.id)) {
                    if (self.analysis.maps.method_map.len() < 10) {
                        println!("Found expr {} {} with id {}", lo_loc.line, *lo_loc.col, ex.id);
                        //TODO is it right to use the method map? Am I using the right id to call it?
                        println("no key for method map:");
                        for (k, v) in self.analysis.maps.method_map.iter() {
                            println!("key: {},", *k);
                            match v.origin {
                                typeck::method_static(id) => println!("static {}", id.node),
                                typeck::method_param(_method_param) => println("Param"),
                                typeck::method_object(_method_object) => println("object"),
                            }
                        }
                    }
                    visit::walk_expr(self, ex, ());
                    return;
                }
                let origin = self.analysis.maps.method_map.get(&ex.id).origin;
                match origin {
                    typeck::method_static(def_id) => {
                        if (def_id.crate == LOCAL_CRATE &&
                            self.analysis.ty_cx.items.contains_key(&def_id.node)) {
                            let item = self.analysis.ty_cx.items.get(&def_id.node);
                            match *item {
                                node_item(item, path) => {
                                    let name = ident_to_str(&item.ident);
                                    println!("call to {}", name);
                                    println!("path: '{}'", path_ident_to_str(path, item.ident, get_ident_interner()));
                                }
                                //TODO
                                _ => ()
                            }
                        } else {
                            //TODO methods from another crate
                            println("non-local or no key");
                        }
                    }
                    // TODO trait methods etc,
                    _ => println("non-static method")
                }
            }*/
            ExprField(sub_ex, ident, _) => {
                self.visit_expr(sub_ex, e);


                let types = self.analysis.ty_cx.node_types.borrow();
                let t = types.get().find(&(sub_ex.id as uint));
                match t {
                    Some(t) => {
                        let t_box = ty::get(*t);
                        match t_box.sty {
                            ty::ty_struct(def_id, _) => {
                                let fields = ty::lookup_struct_fields(self.analysis.ty_cx, def_id);
                                for f in fields.iter() {
                                    if f.name == ident.name {
                                        let sub_span = self.span_for_name(&ex.span);
                                        write!(self.out, "{}",
                                               self.ref_str("var_ref", &ex.span, &sub_span, f.id.node));
                                    }
                                }
                            },
                            _ => println("Expected struct type, but not ty_struct"),
                        }
                    },
                    None => println("No type for sub-expression in field reference"),
                }
            },
            _ => visit::walk_expr(self, ex, e),
        }
    }

    fn visit_pat(&mut self, p:&Pat, e: DxrVisitorEnv) {
        match p.node {
            PatIdent(_, ref path, ref optional_subpattern) => {
                self.collected_paths.push((p.id, path.clone()));
                match *optional_subpattern {
                    None => {}
                    Some(subpattern) => self.visit_pat(subpattern, e),
                }
            }
            _ => visit::walk_pat(self, p, e)
        }
    }

    fn visit_local(&mut self, l:@Local, e: DxrVisitorEnv) {
        // the local could declare multiple new vars, we must walk the pattern and collect them all
        self.visit_pat(l.pat, e);
        for &(id,ref p) in self.collected_paths.iter() {
            // get the span only for the name of the variable (I hope the path is only ever a
            // variable name, but who knows?)
            let sub_span = self.span_for_name(&p.span);
            // for some reason, Rust uses the id of the pattern for var lookups, so we'll
            // use it too
            write!(self.out, "{}",
                   self.variable_str(&p.span,
                                     &sub_span,
                                     id,
                                     path_to_str(p, get_ident_interner())));
        }
        self.collected_paths.clear();

        // Just walk the initialiser and type (don't want to walk the pattern again)
        self.visit_ty(l.ty, e);
        visit::walk_expr_opt(self, l.init, e);
    }
}

#[deriving(Clone)]
struct DxrVisitorEnv {
    cur_mod: NodeId,
}

impl DxrVisitorEnv {
    fn new() -> DxrVisitorEnv {
        DxrVisitorEnv{cur_mod: 0}
    }
    fn new_nested(new_mod: NodeId) -> DxrVisitorEnv {
        DxrVisitorEnv{cur_mod: new_mod}
    }
}

// TODO I want the crate name, not the src_name (not sure how/if they are different)
pub fn process_crate(sess: Session,
                     crate: &ast::Crate,
                     analysis: &CrateAnalysis,
                     odir: &Option<Path>,
                     src_name: &str) {
    println!("Dumping crate {}", src_name);

    // find a path to dump our data to
    let mut root_path = match os::getenv("DXR_RUST_TEMP_FOLDER") {
        Some(val) => Path::new(val),
        None => match *odir {
            Some(ref val) => {
                let mut path = val.clone();
                path.push("dxr");
                path },
            None() => Path::new("~/dxr-temp"),
        },
    };
    
    fs::mkdir_recursive(&root_path, io::UserRWX);
    {
        let disp = root_path.display();
        println!("Writing output to {}", disp);
    }

    //TODO what will happen with a file in subdir? Other crates?
    let src_parts: ~[&str] = src_name.split('.').collect();
    let output_name = if src_parts.len() > 0 {
        src_parts[0].to_owned()
    } else {
        ~"output"
    };

    // create ouput file
    root_path.push(output_name + ".csv");
    let output_file = match File::create(&root_path) {
        Some(f) => ~f,
        None => {
            let disp = root_path.display();
            println!("Could not open {}", disp);
            return;
        }
    };
    root_path.pop();

    let mut visitor = DxrVisitor{sess: sess,
                                 analysis: analysis,
                                 collected_paths: ~[],
                                 out: output_file as ~Writer};
    visit::walk_crate(&mut visitor, crate, DxrVisitorEnv::new());

    // TODO need info about the crate from analysis such as exports?
}
