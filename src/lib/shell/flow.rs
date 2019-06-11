use super::{
    flow_control::{insert_statement, Case, ElseIf, Function, Statement},
    signals,
    status::*,
    Shell,
};
use crate::{
    parser::{
        assignments::is_array,
        parse_and_validate,
        pipelines::{PipeItem, Pipeline},
        Expander, ForValueExpression, StatementSplitter, Terminator,
    },
    shell::Value,
    types,
};
use itertools::Itertools;
use small;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum Condition {
    Continue,
    Break,
    NoOp,
    SigInt,
}

impl<'a> Shell<'a> {
    /// Conditionally executes branches of statements according to evaluated
    /// expressions
    fn execute_if(
        &mut self,
        expression: &[Statement<'a>],
        success: &[Statement<'a>],
        else_if: &[ElseIf<'a>],
        failure: &[Statement<'a>],
    ) -> Condition {
        // Try execute success branch
        if let Condition::SigInt = self.execute_statements(&expression) {
            return Condition::SigInt;
        }
        if self.previous_status.is_success() {
            return self.execute_statements(&success);
        }

        // Try to execute else_if branches
        for ElseIf { expression, success } in else_if {
            if let Condition::SigInt = self.execute_statements(&expression) {
                return Condition::SigInt;
            }

            if self.previous_status.is_success() {
                return self.execute_statements(&success);
            }
        }

        self.execute_statements(&failure)
    }

    /// Executes all of the statements within a for block for each value
    /// specified in the range.
    fn execute_for(
        &mut self,
        variables: &[types::Str],
        values: &[small::String],
        statements: &[Statement<'a>],
    ) -> Condition {
        macro_rules! set_vars_then_exec {
            ($chunk:expr, $def:expr) => {
                for (key, value) in variables.iter().zip($chunk.chain(::std::iter::repeat($def))) {
                    if key != "_" {
                        self.variables_mut().set(key, value.clone());
                    }
                }

                match self.execute_statements(statements) {
                    Condition::Break => break,
                    Condition::SigInt => return Condition::SigInt,
                    Condition::Continue | Condition::NoOp => (),
                }
            };
        }

        let default = ::small::String::new();

        match ForValueExpression::new(values, self) {
            ForValueExpression::Multiple(values) => {
                for chunk in &values.iter().chunks(variables.len()) {
                    set_vars_then_exec!(chunk, &default);
                }
            }
            ForValueExpression::Normal(value) => {
                if &variables[0] != "_" {
                    self.variables_mut().set(&variables[0], value.clone());
                }

                match self.execute_statements(statements) {
                    Condition::SigInt => return Condition::SigInt,
                    Condition::Break | Condition::Continue | Condition::NoOp => (),
                }
            }
            ForValueExpression::Range(range) => {
                for chunk in &range.chunks(variables.len()) {
                    set_vars_then_exec!(chunk, default.clone());
                }
            }
        };

        Condition::NoOp
    }

    /// Executes all of the statements within a while block until a certain
    /// condition is met.
    fn execute_while(
        &mut self,
        expression: &[Statement<'a>],
        statements: &[Statement<'a>],
    ) -> Condition {
        loop {
            self.execute_statements(expression);
            if !self.previous_status.is_success() {
                return Condition::NoOp;
            }

            // Cloning is needed so the statement can be re-iterated again if needed.
            match self.execute_statements(statements) {
                Condition::Break => return Condition::NoOp,
                Condition::SigInt => return Condition::SigInt,
                _ => (),
            }
        }
    }

    /// Executes a single statement
    pub fn execute_statement(&mut self, statement: &Statement<'a>) -> Condition {
        match statement {
            Statement::Let(action) => {
                self.previous_status = self.local(action);
                self.variables.set("?", self.previous_status);
            }
            Statement::Export(action) => {
                self.previous_status = self.export(action);
                self.variables.set("?", self.previous_status);
            }
            Statement::While { expression, statements } => {
                if self.execute_while(&expression, &statements) == Condition::SigInt {
                    return Condition::SigInt;
                }
            }
            Statement::For { variables, values, statements } => {
                if self.execute_for(&variables, &values, &statements) == Condition::SigInt {
                    return Condition::SigInt;
                }
            }
            Statement::If { expression, success, else_if, failure, .. } => {
                let condition = self.execute_if(&expression, &success, &else_if, &failure);

                if condition != Condition::NoOp {
                    return condition;
                }
            }
            Statement::Function { name, args, statements, description } => {
                self.variables.set(
                    &name,
                    Function::new(
                        description.clone(),
                        name.clone(),
                        args.to_vec(),
                        statements.to_vec(),
                    ),
                );
            }
            Statement::Pipeline(pipeline) => match expand_pipeline(&self, &pipeline) {
                Ok((pipeline, statements)) => {
                    if !pipeline.items.is_empty() {
                        self.run_pipeline(pipeline);
                    }
                    if self.opts.err_exit && !self.previous_status.is_success() {
                        self.exit();
                    }
                    if !statements.is_empty() {
                        self.execute_statements(&statements);
                    }
                }
                Err(e) => {
                    self.previous_status =
                        Status::error(format!("ion: pipeline expansion error: {}", e));
                    self.variables.set("?", self.previous_status);
                    self.flow_control.clear();
                    return Condition::Break;
                }
            },
            Statement::Time(box_statement) => {
                let time = std::time::Instant::now();

                let condition = self.execute_statement(box_statement);

                let duration = time.elapsed();
                let seconds = duration.as_secs();
                let nanoseconds = duration.subsec_nanos();

                if seconds > 60 {
                    println!("real    {}m{:02}.{:09}s", seconds / 60, seconds % 60, nanoseconds);
                } else {
                    println!("real    {}.{:09}s", seconds, nanoseconds);
                }
                if condition != Condition::NoOp {
                    return condition;
                }
            }
            Statement::And(box_statement) => {
                let condition = if self.previous_status.is_success() {
                    self.execute_statement(box_statement)
                } else {
                    Condition::NoOp
                };

                if condition != Condition::NoOp {
                    return condition;
                }
            }
            Statement::Or(box_statement) => {
                let condition = if self.previous_status.is_success() {
                    Condition::NoOp
                } else {
                    self.execute_statement(box_statement)
                };

                if condition != Condition::NoOp {
                    return condition;
                }
            }
            Statement::Not(box_statement) => {
                // NOTE: Should the condition be used?
                let _condition = self.execute_statement(box_statement);
                self.previous_status.toggle();
                self.variables.set("?", self.previous_status);
            }
            Statement::Break => return Condition::Break,
            Statement::Continue => return Condition::Continue,
            Statement::Match { expression, cases } => {
                let condition = self.execute_match(expression, &cases);

                if condition != Condition::NoOp {
                    return condition;
                }
            }
            _ => {}
        }
        if let Some(signal) = signals::SignalHandler.next() {
            if self.handle_signal(signal) {
                self.exit_with_code(Status::from_signal(signal));
            }
            Condition::SigInt
        } else if self.break_flow {
            self.break_flow = false;
            Condition::SigInt
        } else {
            Condition::NoOp
        }
    }

    /// Simply executes all supplied statements.
    pub fn execute_statements(&mut self, statements: &[Statement<'a>]) -> Condition {
        self.variables.new_scope(false);

        let condition = statements
            .iter()
            .map(|statement| self.execute_statement(statement))
            .find(|&condition| condition != Condition::NoOp)
            .unwrap_or(Condition::NoOp);

        self.variables.pop_scope();

        condition
    }

    /// Expand an expression and run a branch based on the value of the
    /// expanded expression
    fn execute_match<T: AsRef<str>>(&mut self, expression: T, cases: &[Case<'a>]) -> Condition {
        // Logic for determining if the LHS of a match-case construct (the value we are
        // matching against) matches the RHS of a match-case construct (a value
        // in a case statement). For example, checking to see if the value
        // "foo" matches the pattern "bar" would be invoked like so :
        // ```ignore
        // matches("foo", "bar")
        // ```
        let is_array = is_array(expression.as_ref());
        let value = self.expand_string(expression.as_ref());
        for case in cases.iter() {
            if case
                .value
                .as_ref()
                .map(|v| self.expand_string(&v))
                .filter(|v| v.iter().all(|v| !value.contains(v)))
                .is_none()
            {
                // let pattern_is_array = is_array(&value);
                let previous_bind = case.binding.as_ref().and_then(|bind| {
                    if is_array {
                        let out = if let Some(Value::Array(array)) =
                            self.variables.get_ref(bind).cloned()
                        {
                            Some(Value::Array(array))
                        } else {
                            None
                        };
                        self.variables_mut().set(
                            &bind,
                            value.iter().cloned().map(Value::Str).collect::<types::Array<'_>>(),
                        );
                        out
                    } else {
                        let out = self.variables.get_str(bind).map(Value::Str);
                        self.variables_mut().set(&bind, value.join(" "));
                        out
                    }
                });

                if let Some(statement) = case.conditional.as_ref() {
                    self.on_command(statement);
                    if self.previous_status.is_failure() {
                        continue;
                    }
                }

                let condition = self.execute_statements(&case.statements);

                if let Some(ref bind) = case.binding {
                    if let Some(value) = previous_bind {
                        match value {
                            Value::HashMap(_) | Value::Array(_) | Value::Str(_) => {
                                self.variables_mut().set(bind, value);
                            }
                            _ => (),
                        }
                    }
                }

                return condition;
            }
        }

        Condition::NoOp
    }

    /// Receives a command and attempts to execute the contents.
    pub fn on_command(&mut self, command_string: &str) {
        self.break_flow = false;
        for stmt in command_string.bytes().batching(|cmd| Terminator::new(cmd).terminate()) {
            // Go through all of the statements and build up the block stack
            // When block is done return statement for execution.
            for statement in StatementSplitter::new(&stmt) {
                match statement {
                    Ok(statement) => {
                        let statement = match parse_and_validate(statement, &self.builtins) {
                            Err(why) => {
                                eprintln!("{}", why);
                                self.reset_flow();
                                return;
                            }
                            Ok(r) => r,
                        };
                        match insert_statement(&mut self.flow_control, statement) {
                            Err(why) => {
                                eprintln!("{}", why);
                                self.reset_flow();
                                return;
                            }
                            Ok(Some(stm)) => {
                                let _ = self.execute_statement(&stm);
                            }
                            Ok(None) => {}
                        }
                    }
                    Err(err) => {
                        eprintln!("ion: {}", err);

                        self.previous_status = Status::from_exit_code(-1);
                        self.variables.set("?", self.previous_status);
                        self.flow_control.clear();
                    }
                }
            }
        }
    }
}

/// Expand a pipeline containing aliases. As aliases can split the pipeline by having logical
/// operators in them, the function returns the first half of the pipeline and the rest of the
/// statements, where the last statement has the other half of the pipeline merged.
fn expand_pipeline<'a>(
    shell: &Shell<'a>,
    pipeline: &Pipeline<'a>,
) -> Result<(Pipeline<'a>, Vec<Statement<'a>>), String> {
    let mut item_iter = pipeline.items.iter();
    let mut items: Vec<PipeItem<'a>> = Vec::with_capacity(item_iter.size_hint().0);
    let mut statements = Vec::new();

    while let Some(item) = item_iter.next() {
        if let Some(Value::Alias(alias)) = shell.variables.get_ref(item.command()) {
            statements = StatementSplitter::new(alias.0.as_str())
                .map(|stmt| {
                    stmt.map_err(|why| format!("ion: {}", why)).and_then(|stmt| {
                        parse_and_validate(stmt, &shell.builtins)
                            .map_err(|why| format!("ion: {}", why))
                    })
                })
                .collect::<Result<_, _>>()
                .map_err(|why| format!("ion: {}", why))?;

            // First item in the alias should be a pipeline item, otherwise it cannot
            // be placed into a pipeline!
            let len = statements.len();
            if let Some(Statement::Pipeline(ref mut pline)) = statements.first_mut() {
                // Connect inputs and outputs of alias to pipeline
                if let Some(first) = pline.items.first_mut() {
                    first.inputs = item.inputs.clone();

                    // Add alias arguments to expanded args if there's any.
                    first.job.args.extend(item.job.args.iter().skip(1).cloned());
                }
                if len == 1 {
                    if let Some(mut last) = pline.items.last_mut() {
                        last.outputs = item.outputs.clone();
                        last.job.redirection = item.job.redirection;
                    }
                }
                items.append(&mut pline.items);
                statements.remove(0);
            }

            // Handle pipeline being broken half by i.e.: '&&' or '||'
            if !statements.is_empty() {
                match statements.last_mut().unwrap() {
                    Statement::And(ref mut boxed_stm)
                    | Statement::Or(ref mut boxed_stm)
                    | Statement::Not(ref mut boxed_stm)
                    | Statement::Time(ref mut boxed_stm) => {
                        if let Statement::Pipeline(ref mut pline) = &mut **boxed_stm {
                            // Set output of alias to be the output of last pipeline.
                            if let Some(last) = pline.items.last_mut() {
                                last.outputs = item.outputs.clone();
                                last.job.redirection = item.job.redirection;
                            }
                            // Append rest of the pipeline to the last pipeline in the
                            // alias.
                            pline.items.extend(item_iter.cloned());
                        } else {
                            // Error in expansion
                            return Err(format!(
                                "unable to pipe outputs of alias: '{} = {}'",
                                item.command(),
                                alias.0.as_str()
                            ));
                        }
                    }
                    _ => (),
                }
                break;
            }
        } else {
            items.push(item.clone());
        }
    }
    Ok((Pipeline { items, pipe: pipeline.pipe }, statements))
}
