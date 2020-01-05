mod decl;
mod expr;
mod stmt;

use std::collections::VecDeque;
use std::fmt;
use std::iter::Iterator;
use std::mem;

use crate::data::{prelude::*, Scope};
use crate::utils::warn;

type Lexeme = CompileResult<Locatable<Token>>;
pub(crate) type TagScope = Scope<InternedStr, TagEntry>;

#[derive(Clone, Debug)]
pub(crate) enum TagEntry {
    Struct(StructRef),
    Union(StructRef),
    // list of (name, value)s
    Enum(Vec<(InternedStr, i64)>),
}

#[derive(Debug)]
pub struct Parser<I: Iterator<Item = Lexeme>> {
    /// C actually has 4 different scopes:
    /// - label names
    /// - tags
    /// - members
    /// - ordinary identifiers
    ///
    /// This holds the scope for ordinary identifiers: variables and typedefs
    scope: Scope<InternedStr, Symbol>,
    /// the compound types that have been declared (struct/union/enum)
    tag_scope: TagScope,
    /// we iterate lazily over the tokens, so if we have a program that's mostly valid but
    /// breaks at the end, we don't only show lex errors
    tokens: I,
    /// VecDeque supports pop_front with reasonable efficiency
    /// this is useful because errors are FIFO
    pending: VecDeque<Locatable<Declaration>>,
    /// in case we get to the end of the file and want to show an error
    last_location: Location,
    /// the last token we saw from the Lexer. None if we haven't looked ahead.
    /// Should only be used in this module.
    current: Option<Locatable<Token>>,
    /// TODO: are we sure we need 2 tokens of lookahead?
    /// this was put here for declarations, so we know the difference between
    /// int (*x) and int (int), but there's probabsly a workaround
    next: Option<Locatable<Token>>,
    /// the function we are currently compiling.
    /// if `None`, we are in global scope.
    /// used for checking return types
    current_function: Option<FunctionData>,
    /// whether to debug each declaration
    debug: bool,
    error_handler: ErrorHandler,
}

#[derive(Debug)]
/// used to keep track of function metadata
/// while doing semantic analysis
struct FunctionData {
    /// the name of the function
    id: InternedStr,
    /// where the function was declared
    location: Location,
    /// the return type of the function
    return_type: Type,
}

impl<I> Parser<I>
where
    I: Iterator<Item = Lexeme>,
{
    /// If the input is not empty,
    ///     If there is at least one token that is not an error, returns a parser.
    ///     Otherwise, returns a list of the errors.
    /// Otherwise, returns None.
    pub fn new(mut iter: I, debug: bool) -> Result<Self, VecDeque<CompileError>> {
        let mut error_handler = ErrorHandler::new();
        let mut pending = VecDeque::new();
        let first = loop {
            match iter.next() {
                Some(Ok(token)) => break token,
                Some(Err(err)) => pending.push_back(err),
                None if pending.is_empty() => {
                    error_handler.push_err(CompileError::semantic(Locatable::new(
                        "cannot have empty program".to_string(),
                        Default::default(),
                    )));
                    return Err(pending);
                }
                None => return Err(pending),
            }
        };
        if !pending.is_empty() {
            return Err(pending);
        }
        Ok(Parser {
            scope: Scope::new(),
            tag_scope: Scope::new(),
            tokens: iter,
            pending: Default::default(),
            last_location: first.location,
            current: Some(first),
            next: None,
            current_function: None,
            debug,
            error_handler,
        })
    }
}

impl<I: Iterator<Item = Lexeme>> Iterator for Parser<I> {
    type Item = CompileResult<Locatable<Declaration>>;
    /// translation_unit
    /// : external_declaration
    /// | translation_unit external_declaration
    /// ;
    ///
    /// external_declaration
    /// : function_definition
    /// | declaration
    /// ;
    ///
    /// function_definition
    /// : declarator compound_statement
    /// | declaration_specifiers declarator compound_statement
    /// ;
    fn next(&mut self) -> Option<Self::Item> {
        let next = self
            .pending
            .pop_front()
            .map(Result::Ok)
            .or_else(|| self.error_handler.pop_err().map(Result::Err))
            .or_else(|| {
                // Parse more of our file

                // Remove extra semicolons
                while let Some(locatable) = self.match_next(&Token::Semicolon) {
                    warn("extraneous semicolon at top level", locatable.location);
                }

                // Check for end of file
                if self.peek_token().is_none() {
                    if self.scope.is_global() {
                        self.leave_scope(self.last_location);
                    }

                    if self.error_handler.is_empty() {
                        None
                    } else {
                        self.next()
                    }
                } else {
                    match self.declaration() {
                        Ok(decls) => {
                            self.pending.extend(decls.into_iter());
                        }
                        Err(err) => {
                            self.error_handler.push_err(err.into());
                        }
                    }

                    self.next()
                }
            });
        if self.debug {
            if let Some(Ok(decl)) = &next {
                println!("{}", decl.data);
            }
        }
        next
    }
}

impl<I: Iterator<Item = Lexeme>> Parser<I> {
    /* utility functions */
    #[inline]
    fn enter_scope(&mut self) {
        self.scope.enter_scope();
        self.tag_scope.enter_scope();
    }
    fn leave_scope(&mut self, location: Location) {
        use crate::data::StorageClass;
        for object in self.scope.get_all_immediate().values() {
            match &object.ctype {
                Type::Struct(StructType::Named(name, members))
                | Type::Union(StructType::Named(name, members)) => {
                    if members.get().is_empty()
                        && object.storage_class != StorageClass::Extern
                        && object.storage_class != StorageClass::Typedef
                    {
                        self.error_handler
                            .push_err(CompileError::semantic(Locatable {
                                data: format!(
                                    "forward declaration of {} is never completed (used in {})",
                                    name, object.id
                                ),
                                location,
                            }));
                    }
                }
                _ => {}
            }
        }
        self.scope.leave_scope();
        self.tag_scope.leave_scope();
    }
    // don't use this, use next_token instead
    fn __impl_next_token(&mut self) -> Option<Locatable<Token>> {
        loop {
            match self.tokens.next() {
                Some(Ok(token)) => {
                    self.last_location = token.location;
                    break Some(token);
                }
                Some(Err(err)) => {
                    self.last_location = err.location();
                    self.lex_error(err);
                }
                None => break None,
            }
        }
    }
    fn next_token(&mut self) -> Option<Locatable<Token>> {
        if self.current.is_some() {
            let tmp = mem::replace(&mut self.next, None);
            mem::replace(&mut self.current, tmp)
        } else {
            self.__impl_next_token()
        }
    }
    fn peek_token(&mut self) -> Option<&Token> {
        if self.current.is_none() {
            self.current = self.next.take().or_else(|| self.next_token());
        }
        self.current.as_ref().map(|x| &x.data)
    }
    // TODO: this is mostly copied from peek_token
    fn peek_next_token(&mut self) -> Option<&Token> {
        if self.next.is_none() {
            if self.current.is_none() {
                self.current = self.__impl_next_token();
            }
            self.next = self.__impl_next_token();
        }
        self.next.as_ref().map(|x| &x.data)
    }
    fn next_location(&mut self) -> Location {
        if let Some(token) = &self.current {
            token.location
        } else {
            self.last_location
        }
    }
    fn match_next(&mut self, next: &Token) -> Option<Locatable<Token>> {
        self.match_any(&[next])
    }
    fn match_any(&mut self, choices: &[&Token]) -> Option<Locatable<Token>> {
        if let Some(data) = self.peek_token() {
            for token in choices {
                if token.same_kind(data) {
                    return self.next_token();
                }
            }
        }
        None
    }
    /* error handling functions */
    fn semantic_err<S: Into<String>>(&mut self, msg: S, location: Location) {
        self.error_handler
            .push_err(CompileError::semantic(Locatable {
                location,
                data: msg.into(),
            }));
    }
    /*
     * If we're in an invalid state, try to recover.
     * Consume tokens until the end of a statement - either ';' or '}'
     */
    fn panic(&mut self) {
        while let Some(token) = self.next_token() {
            match token.data {
                Token::Semicolon => break,
                Token::RightBrace => {
                    break;
                }
                _ => continue,
            };
        }
    }
    fn expect(&mut self, next: Token) -> Result<Locatable<Token>, SyntaxError> {
        let token = match self.peek_token() {
            Some(t) => t,
            None => {
                let err = Err(Locatable {
                    location: self.last_location, // TODO: we don't actually want this, we want the end of the file
                    data: format!("expected '{}', got '<end-of-file>'", next),
                }
                .into());
                self.panic();
                return err;
            }
        };
        if token.same_kind(&next) {
            Ok(self.next_token().unwrap())
        } else {
            let err = Err(Locatable {
                data: format!("expected '{}', got '{}'", next, token),
                location: self.next_location(),
            }
            .into());
            self.panic();
            err
        }
    }
    /// replace `self.current` with `item`
    /// replace `self.next` with `self.current`
    /// the previous value of `self.next` is lost
    fn unput(&mut self, item: Option<Locatable<Token>>) {
        let tmp = mem::replace(&mut self.current, item);
        mem::replace(&mut self.next, tmp);
    }
    fn lex_error(&mut self, err: CompileError) {
        self.error_handler.push_err(err);
    }
    pub fn collect_results(self) -> (Vec<Locatable<Declaration>>, Vec<CompileError>) {
        let mut decls = Vec::new();
        let mut errs = Vec::new();
        for result in self {
            match result {
                Ok(decl) => decls.push(decl),
                Err(err) => errs.push(err),
            }
        }
        (decls, errs)
    }
}

impl std::fmt::Display for TagEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TagEntry::Enum(_) => write!(f, "enum"),
            TagEntry::Struct(_) => write!(f, "struct"),
            TagEntry::Union(_) => write!(f, "union"),
        }
    }
}

impl Token {
    fn same_kind(&self, other: &Self) -> bool {
        match (self, other) {
            // special case keywords, assignments, and comparisons - they must match exactly
            (Token::Keyword(left), Token::Keyword(right)) => left == right,
            (Token::Assignment(left), Token::Assignment(right)) => left == right,
            (Token::Comparison(left), Token::Comparison(right)) => left == right,
            // in any other case, we're just checking they're the same enum variant
            _ => mem::discriminant(self) == mem::discriminant(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Lexeme;
    use super::Parser;
    use crate::data::prelude::*;
    use crate::lex::Lexer;

    pub(crate) type ParseType = CompileResult<Locatable<Declaration>>;
    pub(crate) fn parse(input: &str) -> Option<ParseType> {
        let mut all = parse_all(input);
        match all.len() {
            0 => None,
            1 => Some(all.remove(0)),
            n => Some(Err(CompileError::semantic(Locatable {
                location: match all.remove(1) {
                    Ok(x) => x.location,
                    Err(x) => x.location(),
                },
                data: format!("Expected exactly one statement, got {}", n),
            }))),
        }
    }
    pub(crate) fn assert_errs_decls(input: &str, errs: usize, decls: usize) {
        let parser = parser(input);
        let (decl_iter, err_iter) = parser.collect_results();
        assert!(
            (err_iter.len(), decl_iter.len()) == (errs, decls),
            "({} errs, {} decls) != ({}, {}) when parsing {}",
            err_iter.len(),
            decl_iter.len(),
            errs,
            decls,
            input
        );
    }
    #[inline]
    pub(crate) fn parse_all(input: &str) -> Vec<ParseType> {
        parser(input).collect()
    }
    #[inline]
    pub(crate) fn match_data<T>(lexed: Option<ParseType>, closure: T) -> bool
    where
        T: Fn(Declaration) -> bool,
    {
        match lexed {
            Some(Ok(decl)) => closure(decl.data),
            _ => false,
        }
    }
    #[inline]
    pub(crate) fn match_all<I, T>(mut lexed: I, closure: T) -> bool
    where
        I: Iterator<Item = ParseType>,
        T: Fn(Declaration) -> bool,
    {
        lexed.all(|l| match l {
            Ok(decl) => closure(decl.data),
            _ => false,
        })
    }
    #[inline]
    pub(crate) fn parser(input: &str) -> Parser<impl Iterator<Item = Lexeme>> {
        let tokens =
            Lexer::new("<test suite>".to_string(), input.chars(), false).collect::<Vec<_>>();
        Parser::new(tokens.into_iter(), false).unwrap()
    }
    #[test]
    fn peek() {
        use crate::data::lex::{Keyword, Token};
        use crate::intern::InternedStr;
        let mut instance = parser("int a[(int)1];");
        assert_eq!(
            instance.next_token().unwrap().data,
            Token::Keyword(Keyword::Int)
        );
        assert_eq!(
            instance.next_token().unwrap().data,
            Token::Id(InternedStr::get_or_intern("a"))
        );
        assert_eq!(instance.peek_token(), Some(&Token::LeftBracket));
        assert_eq!(instance.peek_next_token(), Some(&Token::LeftParen));
        assert_eq!(instance.peek_token(), Some(&Token::LeftBracket));
        assert_eq!(instance.next_token().unwrap().data, Token::LeftBracket);
        assert_eq!(instance.next_token().unwrap().data, Token::LeftParen);
        assert_eq!(
            instance.next_token().unwrap().data,
            Token::Keyword(Keyword::Int)
        );
    }
    #[test]
    fn multiple_declaration() {
        let mut decls = parse_all("int a; int a;");
        assert_eq!(decls.len(), 2, "{:?}", decls);
        assert!(decls.pop().unwrap().is_ok());
        assert!(decls.pop().unwrap().is_ok());
        assert_errs_decls("int a; char *a[];", 1, 2);
    }
    #[test]
    fn semicolons() {
        let mut buf = Vec::new();
        buf.resize(10_000, ';');
        let buf: String = buf.into_iter().collect();
        assert!(parse(&buf).is_none());
    }
}
