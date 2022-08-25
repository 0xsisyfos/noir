use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::{
    hir_def::{
        expr::*,
        function::Parameters,
        stmt::{HirAssignStatement, HirLValue, HirPattern, HirStatement},
    },
    node_interner::{self, NodeInterner, StmtId},
    util::vecmap,
    IsConst, TypeBinding, TypeBindings,
};

use self::ast::{DefinitionId, FuncId, Functions};

pub mod ast;

struct Monomorphiser {
    // Store monomorphised globals and locals separately,
    // only locals are cleared on each function call and only globals are monomorphised.
    // Nested HashMaps in globals lets us avoid cloning HirTypes when calling .get()
    globals: HashMap<node_interner::FuncId, HashMap<HirType, FuncId>>,
    locals: HashMap<node_interner::DefinitionId, DefinitionId>,

    /// Queue of functions to monomorphise next
    queue: VecDeque<(node_interner::FuncId, FuncId, TypeBindings)>,

    interner: NodeInterner,

    next_local_id: u32,
    next_function_id: u32,
}

type HirType = crate::Type;

pub fn monomorphise(main: node_interner::FuncId, interner: NodeInterner) -> Functions {
    let mut monomorphiser = Monomorphiser::new(interner);
    let mut functions = monomorphiser.compile_main(main);

    while !monomorphiser.queue.is_empty() {
        let (next_fn_id, new_id, bindings) = monomorphiser.queue.pop_front().unwrap();
        monomorphiser.locals.clear();

        perform_instantiation_bindings(&bindings);
        functions.push(monomorphiser.function(next_fn_id, new_id));
        undo_instantiation_bindings(bindings);
    }

    functions
}

impl Monomorphiser {
    fn new(interner: NodeInterner) -> Monomorphiser {
        Monomorphiser {
            globals: HashMap::new(),
            locals: HashMap::new(),
            queue: VecDeque::new(),
            next_local_id: 0,
            next_function_id: 1,
            interner,
        }
    }

    fn next_definition_id(&mut self) -> DefinitionId {
        let id = self.next_local_id;
        self.next_local_id += 1;
        DefinitionId(id)
    }

    fn next_function_id(&mut self) -> ast::FuncId {
        let id = self.next_function_id;
        self.next_function_id += 1;
        ast::FuncId(id)
    }

    fn lookup_local(&mut self, id: node_interner::DefinitionId) -> Option<DefinitionId> {
        self.locals.get(&id).copied()
    }

    fn lookup_global(&mut self, id: node_interner::FuncId, typ: &HirType) -> Option<FuncId> {
        self.globals.get(&id).and_then(|inner_map| inner_map.get(typ)).copied()
    }

    fn define_local(&mut self, id: node_interner::DefinitionId, new_id: DefinitionId) {
        self.locals.insert(id, new_id);
    }

    fn define_global(&mut self, id: node_interner::FuncId, typ: HirType, new_id: FuncId) {
        self.globals.entry(id).or_default().insert(typ, new_id);
    }

    /// The main function is special, we need to check for a return type and if present,
    /// insert an extra constrain on the return value.
    fn compile_main(&mut self, main_id: node_interner::FuncId) -> Functions {
        let mut main = self.function(main_id, FuncId(0));
        let main_meta = self.interner.function_meta(&main_id);

        if main.return_type != ast::Type::Unit {
            let id = self.next_definition_id();

            main.parameters.push((id, main.return_type, "return".into()));
            main.return_type = ast::Type::Unit;

            let name = "_".into();
            let typ = self.convert_type(main_meta.return_type());
            let lhs =
                Box::new(ast::Expression::Ident(ast::Ident { id, location: None, name, typ }));
            let rhs = Box::new(main.body);
            let operator = ast::BinaryOp::Equal;
            let eq = ast::Expression::Binary(ast::Binary { operator, lhs, rhs });

            let location = self.interner.function_meta(&main_id).location;
            main.body = ast::Expression::Constrain(Box::new(eq), location);
        }

        let abi = main_meta.into_abi(&self.interner);
        Functions::new(main, abi)
    }

    fn function(&mut self, f: node_interner::FuncId, id: FuncId) -> ast::Function {
        let meta = self.interner.function_meta(&f);
        let name = self.interner.function_name(&f).to_owned();

        let return_type = self.convert_type(meta.return_type());
        let parameters = self.parameters(meta.parameters);
        let body = self.expr_infer(*self.interner.function(&f).as_expr());

        ast::Function { id, name, parameters, body, return_type }
    }

    /// Monomorphise each parameter, expanding tuple/struct patterns into multiple parameters
    /// and binding any generic types found.
    fn parameters(&mut self, params: Parameters) -> Vec<(ast::DefinitionId, ast::Type, String)> {
        let mut new_params = Vec::with_capacity(params.len());
        for parameter in params {
            self.parameter(parameter.0, &parameter.1, &mut new_params);
        }
        new_params
    }

    fn parameter(
        &mut self,
        param: HirPattern,
        typ: &HirType,
        new_params: &mut Vec<(ast::DefinitionId, ast::Type, String)>,
    ) {
        match param {
            HirPattern::Identifier(ident) => {
                //let value = self.expand_parameter(typ, new_params);
                let new_id = self.next_definition_id();
                let name = self.interner.definition_name(ident.id).to_owned();
                new_params.push((new_id, self.convert_type(typ), name));
                self.define_local(ident.id, new_id);
            }
            HirPattern::Mutable(pattern, _) => self.parameter(*pattern, typ, new_params),
            HirPattern::Tuple(fields, _) => {
                let tuple_field_types = unwrap_tuple_type(typ);

                for (field, typ) in fields.into_iter().zip(tuple_field_types) {
                    self.parameter(field, &typ, new_params);
                }
            }
            HirPattern::Struct(_, fields, _) => {
                let struct_field_types = unwrap_struct_type(typ);

                for (name, field) in fields {
                    let typ = &struct_field_types[&name.0.contents];
                    self.parameter(field, typ, new_params);
                }
            }
        }
    }

    fn expr_infer(&mut self, expr: node_interner::ExprId) -> ast::Expression {
        let expected_type = self.interner.id_type(expr);
        self.expr(expr, &expected_type)
    }

    fn expr(&mut self, expr: node_interner::ExprId, typ: &HirType) -> ast::Expression {
        use ast::Expression::Literal;
        use ast::Literal::*;

        match self.interner.expression(&expr) {
            HirExpression::Ident(ident) => ast::Expression::Ident(self.ident(ident)),
            HirExpression::Literal(HirLiteral::Str(contents)) => Literal(Str(contents)),
            HirExpression::Literal(HirLiteral::Bool(value)) => Literal(Bool(value)),
            HirExpression::Literal(HirLiteral::Integer(value)) => {
                let typ = self.convert_type(&self.interner.id_type(expr));
                Literal(Integer(value, typ))
            }
            HirExpression::Literal(HirLiteral::Array(array)) => {
                let element_type = self.convert_type(&self.interner.id_type(array.contents[0]));
                let contents = vecmap(array.contents, |id| self.expr_infer(id));
                Literal(Array(ast::ArrayLiteral { length: array.length, contents, element_type }))
            }
            HirExpression::Block(block) => self.block(block.0),

            HirExpression::Prefix(prefix) => ast::Expression::Unary(ast::Unary {
                operator: prefix.operator,
                rhs: Box::new(self.expr_infer(prefix.rhs)),
            }),

            HirExpression::Infix(infix) => {
                let lhs = Box::new(self.expr_infer(infix.lhs));
                let rhs = Box::new(self.expr_infer(infix.rhs));
                let operator = infix.operator.kind;
                ast::Expression::Binary(ast::Binary { lhs, rhs, operator })
            }

            HirExpression::Index(index) => ast::Expression::Index(ast::Index {
                collection: Box::new(self.expr_infer(index.collection)),
                index: Box::new(self.expr_infer(index.index)),
            }),

            HirExpression::MemberAccess(access) => {
                let expr = Box::new(self.expr_infer(access.lhs));
                ast::Expression::ExtractTupleField(expr, access.field_index.unwrap())
            }

            HirExpression::Call(call) => self.function_call(call, expr),

            HirExpression::Cast(cast) => ast::Expression::Cast(ast::Cast {
                lhs: Box::new(self.expr_infer(cast.lhs)),
                r#type: self.convert_type(&cast.r#type),
            }),

            HirExpression::For(for_expr) => {
                let start = self.expr_infer(for_expr.start_range);
                let end = self.expr_infer(for_expr.end_range);
                let index_variable = self.next_definition_id();
                self.define_local(for_expr.identifier.id, index_variable);

                let block = Box::new(self.expr_infer(for_expr.block));

                ast::Expression::For(ast::For {
                    index_variable,
                    index_name: self.interner.definition_name(for_expr.identifier.id).to_owned(),
                    index_type: self.convert_type(&self.interner.id_type(for_expr.start_range)),
                    start_range: Box::new(start),
                    end_range: Box::new(end),
                    block,
                })
            }

            HirExpression::If(if_expr) => {
                let cond = self.expr(if_expr.condition, &HirType::Bool(IsConst::No(None)));
                let then = self.expr(if_expr.consequence, typ);
                let else_ = if_expr.alternative.map(|alt| Box::new(self.expr(alt, typ)));
                ast::Expression::If(ast::If {
                    condition: Box::new(cond),
                    consequence: Box::new(then),
                    alternative: else_,
                })
            }

            HirExpression::Tuple(fields) => {
                let fields = vecmap(fields, |id| self.expr(id, typ));
                ast::Expression::Tuple(fields)
            }
            HirExpression::Constructor(constructor) => self.constructor(constructor, typ),

            HirExpression::MethodCall(_) | HirExpression::Error => unreachable!(),
        }
    }

    fn statement(&mut self, id: StmtId) -> ast::Expression {
        match self.interner.statement(&id) {
            HirStatement::Let(let_statement) => {
                let expr = self.expr_infer(let_statement.expression);
                self.unpack_pattern(let_statement.pattern, expr, &let_statement.r#type)
            }
            HirStatement::Constrain(constrain) => {
                let expr = self.expr(constrain.0, &HirType::Bool(IsConst::No(None)));
                let location = self.interner.expr_location(&constrain.0);
                ast::Expression::Constrain(Box::new(expr), location)
            }
            HirStatement::Assign(assign) => self.assign(assign),
            HirStatement::Expression(expr) => self.expr_infer(expr),
            HirStatement::Semi(expr) => ast::Expression::Semi(Box::new(self.expr_infer(expr))),
            HirStatement::Error => unreachable!(),
        }
    }

    fn constructor(
        &mut self,
        constructor: HirConstructorExpression,
        typ: &HirType,
    ) -> ast::Expression {
        let field_types = unwrap_struct_type(typ);

        // Create let bindings for each field value first to preserve evaluation order before
        // they are reordered and packed into the resulting tuple
        let mut field_vars = BTreeMap::new();
        let mut new_exprs = Vec::with_capacity(constructor.fields.len());

        for (field_name, expr_id) in constructor.fields {
            let new_id = self.next_definition_id();
            let field_type = field_types.get(&field_name.0.contents).unwrap();
            let typ = self.convert_type(field_type);

            field_vars.insert(field_name.0.contents.clone(), (new_id, typ));
            let expression = Box::new(self.expr(expr_id, field_type));

            new_exprs.push(ast::Expression::Let(ast::Let {
                id: new_id,
                name: field_name.0.contents,
                expression,
            }));
        }

        let sorted_fields = vecmap(field_vars, |(name, (id, typ))| {
            ast::Expression::Ident(ast::Ident { id, location: None, name, typ })
        });

        // Finally we can return the created Tuple from the new block
        new_exprs.push(ast::Expression::Tuple(sorted_fields));
        ast::Expression::Block(new_exprs)
    }

    fn block(&mut self, statement_ids: Vec<StmtId>) -> ast::Expression {
        ast::Expression::Block(vecmap(statement_ids, |id| self.statement(id)))
    }

    fn unpack_pattern(
        &mut self,
        pattern: HirPattern,
        value: ast::Expression,
        typ: &HirType,
    ) -> ast::Expression {
        match pattern {
            HirPattern::Identifier(ident) => {
                let new_id = self.next_definition_id();
                self.define_local(ident.id, new_id);
                ast::Expression::Let(ast::Let {
                    id: new_id,
                    name: self.interner.definition_name(ident.id).to_owned(),
                    expression: Box::new(value),
                })
            }
            HirPattern::Mutable(pattern, _) => self.unpack_pattern(*pattern, value, typ),
            HirPattern::Tuple(patterns, _) => {
                let fields = unwrap_tuple_type(typ);
                self.unpack_tuple_pattern(value, patterns.into_iter().zip(fields))
            }
            HirPattern::Struct(_, patterns, _) => {
                let fields = unwrap_struct_type(typ);
                let patterns = patterns.into_iter().map(|(ident, pattern)| {
                    let typ = fields[&ident.0.contents].clone();
                    (pattern, typ)
                });
                self.unpack_tuple_pattern(value, patterns)
            }
        }
    }

    fn unpack_tuple_pattern(
        &mut self,
        value: ast::Expression,
        fields: impl Iterator<Item = (HirPattern, HirType)>,
    ) -> ast::Expression {
        let fresh_id = self.next_definition_id();

        let mut definitions = vec![ast::Expression::Let(ast::Let {
            id: fresh_id,
            name: "_".into(),
            expression: Box::new(value),
        })];

        for (i, (field_pattern, field_type)) in fields.into_iter().enumerate() {
            let typ = self.convert_type(&field_type);
            let name = i.to_string();
            let new_rhs =
                ast::Expression::Ident(ast::Ident { location: None, id: fresh_id, name, typ });
            let new_rhs = ast::Expression::ExtractTupleField(Box::new(new_rhs), i);
            let new_expr = self.unpack_pattern(field_pattern, new_rhs, &field_type);
            definitions.push(new_expr);
        }

        ast::Expression::Block(definitions)
    }

    fn ident(&mut self, ident: HirIdent) -> ast::Ident {
        let id = self.lookup_local(ident.id).unwrap();
        let name = self.interner.definition_name(ident.id).to_owned();
        let typ = self.convert_type(&self.interner.id_type(ident.id));
        ast::Ident { location: Some(ident.location), id, name, typ }
    }

    /// Convert a non-tuple/struct type to a monomorphised type
    fn convert_type(&self, typ: &HirType) -> ast::Type {
        match typ {
            HirType::FieldElement(_) => ast::Type::Field,
            HirType::Integer(_, sign, bits) => ast::Type::Integer(*sign, *bits),
            HirType::Bool(_) => ast::Type::Bool,
            HirType::Unit => ast::Type::Unit,

            HirType::Array(size, element) => {
                let size = size.array_length().unwrap();
                let element = self.convert_type(element.as_ref());
                ast::Type::Array(size, Box::new(element))
            }

            HirType::PolymorphicInteger(_, binding) | HirType::TypeVariable(binding) => {
                match &*binding.borrow() {
                    TypeBinding::Bound(binding) => self.convert_type(binding),
                    TypeBinding::Unbound(_) => unreachable!(),
                }
            }

            HirType::Struct(def, args) => {
                let fields = def.borrow().get_fields(args);
                let fields = vecmap(fields, |(_, field)| self.convert_type(&field));
                ast::Type::Tuple(fields)
            }

            HirType::Tuple(fields) => {
                let fields = vecmap(fields, |field| self.convert_type(field));
                ast::Type::Tuple(fields)
            }

            HirType::Function(_, _, _)
            | HirType::Forall(_, _)
            | HirType::ArrayLength(_)
            | HirType::Error => unreachable!("Unexpected type {} found", typ),
        }
    }

    fn function_call(
        &mut self,
        call: HirCallExpression,
        expr_id: node_interner::ExprId,
    ) -> ast::Expression {
        let typ = self.interner.function_type(expr_id).follow_bindings();

        let func_id = self
            .lookup_global(call.func_id, &typ)
            .unwrap_or_else(|| self.queue_function(call.func_id, expr_id, typ));

        let arguments = vecmap(call.arguments, |id| self.expr_infer(id));
        ast::Expression::Call(ast::Call { func_id, arguments })
    }

    fn queue_function(
        &mut self,
        id: node_interner::FuncId,
        expr_id: node_interner::ExprId,
        function_type: HirType,
    ) -> FuncId {
        let new_id = self.next_function_id();

        self.define_global(id, function_type, new_id);
        let bindings = self.interner.get_instantiation_bindings(expr_id).clone();
        self.queue.push_back((id, new_id, bindings));
        new_id
    }

    fn assign(&mut self, assign: HirAssignStatement) -> ast::Expression {
        let expression = Box::new(self.expr_infer(assign.expression));
        let typ = self.interner.id_type(assign.expression);
        let lvalue = self.lvalue(assign.lvalue, &typ);
        ast::Expression::Assign(ast::Assign { lvalue, expression })
    }

    fn lvalue(&mut self, lvalue: HirLValue, typ: &HirType) -> ast::LValue {
        match lvalue {
            HirLValue::Ident(ident) => {
                let ident = self.ident(ident);
                ast::LValue::Ident(ident)
            }
            HirLValue::MemberAccess { object, field_name } => {
                let field_types = unwrap_struct_type(typ);
                let field_type = &field_types[&field_name.0.contents];
                let object = Box::new(self.lvalue(*object, field_type));
                let field_index = get_field_index(&field_name.0.contents, field_types);
                ast::LValue::MemberAccess { object, field_index }
            }
            HirLValue::Index { array, index } => {
                let element_type = unwrap_array_element_type(typ);
                let array = Box::new(self.lvalue(*array, &element_type));
                let index = Box::new(self.expr_infer(index));
                ast::LValue::Index { array, index }
            }
        }
    }
}

fn get_field_index(field: &str, field_types: BTreeMap<String, crate::Type>) -> usize {
    for (i, name) in field_types.keys().enumerate() {
        if field == name {
            return i;
        }
    }
    unreachable!()
}

fn unwrap_array_element_type(typ: &HirType) -> HirType {
    match typ {
        HirType::Array(_, elem) => elem.as_ref().clone(),
        HirType::TypeVariable(binding) => match &*binding.borrow() {
            TypeBinding::Bound(binding) => unwrap_array_element_type(binding),
            TypeBinding::Unbound(_) => unreachable!(),
        },
        _ => unreachable!(),
    }
}

fn unwrap_tuple_type(typ: &HirType) -> Vec<HirType> {
    match typ {
        HirType::Tuple(fields) => fields.clone(),
        HirType::TypeVariable(binding) => match &*binding.borrow() {
            TypeBinding::Bound(binding) => unwrap_tuple_type(binding),
            TypeBinding::Unbound(_) => unreachable!(),
        },
        _ => unreachable!(),
    }
}

fn unwrap_struct_type(typ: &HirType) -> BTreeMap<String, HirType> {
    match typ {
        HirType::Struct(def, args) => def.borrow().get_fields(args),
        HirType::TypeVariable(binding) => match &*binding.borrow() {
            TypeBinding::Bound(binding) => unwrap_struct_type(binding),
            TypeBinding::Unbound(_) => unreachable!(),
        },
        _ => unreachable!(),
    }
}

fn perform_instantiation_bindings(bindings: &TypeBindings) {
    for (var, binding) in bindings.values() {
        *var.borrow_mut() = TypeBinding::Bound(binding.clone());
    }
}

fn undo_instantiation_bindings(bindings: TypeBindings) {
    for (id, (var, _)) in bindings {
        *var.borrow_mut() = TypeBinding::Unbound(id);
    }
}
