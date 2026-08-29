; mina 構文ハイライト用クエリ (Rust)
;
; capture 名は HighlightGroup の小文字名に一致させる (ADR-0018 フラット taxonomy):
;   comment keyword string number constant function type parameter field
;   operator punctuation attribute error
; 自前実装 (ADR-0001: Helix/Neovim 等からのコピーはしない)。

; comment
(line_comment) @comment
(block_comment) @comment

; string
(string_literal) @string
(raw_string_literal) @string
(char_literal) @string

; number
(integer_literal) @number
(float_literal) @number

; constant (真偽値・const 定義の名前)
["true" "false"] @constant
(const_item name: (identifier) @constant)

; keyword
; 匿名トークン: 一般的なキーワード。
[
  "as" "async" "await" "break" "const" "continue" "dyn" "else"
  "enum" "extern" "fn" "for" "if" "impl" "in" "let" "loop" "match" "mod"
  "move" "pub" "ref" "return" "static" "struct" "trait" "type"
  "unsafe" "use" "where" "while"
] @keyword

; 名前付きトークン (匿名文字列パターンではマッチできない):
;   crate/self/super は専用トークン、mut は mutable_specifier。
;   union は reserved identifier (実体が identifier) のため対象外 (M1)。
(crate) @keyword
(super) @keyword
(self) @keyword
(mutable_specifier) @keyword

; function (定義・呼び出し・マクロ)
(function_item name: (identifier) @function)
(function_signature_item name: (identifier) @function)
(call_expression function: (identifier) @function)
; scoped パス呼び出し (HashMap::new() / foo::bar())。scoped_identifier の
; name フィールド (= 最後のセグメント) を関数呼び出しとして捕捉する。
(call_expression function: (scoped_identifier name: (identifier) @function))
(call_expression function: (field_expression field: (field_identifier) @function))
(macro_invocation macro: (identifier) @function)

; type
(type_identifier) @type
(primitive_type) @type

; parameter
(parameter pattern: (identifier) @parameter)

; field
(field_identifier) @field

; operator
[
  "+" "-" "*" "/" "%" "=" "==" "!=" "<" ">" "<=" ">=" "&&" "||" "!" "&"
  "|" "^" "<<" ">>" "+=" "-=" "*=" "/=" "%=" "&=" "|=" "^=" "->" "=>" "::"
] @operator

; punctuation (括弧・区切り)
[
  "(" ")" "[" "]" "{" "}" "," ";" ":"
] @punctuation

; attribute
(attribute_item) @attribute

; error (未構文)
(ERROR) @error
