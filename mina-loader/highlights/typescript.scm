; mina 構文ハイライト用クエリ (TypeScript)
;
; capture 名は HighlightGroup の小文字名に一致させる (ADR-0018 フラット taxonomy):
;   comment keyword string number constant function type parameter field
;   operator punctuation attribute error
; 自前実装 (ADR-0001: Helix/Neovim 等からのコピーはしない)。
; ノード名は tree-sitter-typescript (transpiled grammar) の node-types から引く。

; comment
(comment) @comment
(html_comment) @comment

; string
(string) @string
(template_string) @string
(regex) @string

; number
(number) @number

; constant (真偽値・null・undefined — named トークンのため (node) 形式)
(true) @constant
(false) @constant
(null) @constant
(undefined) @constant

; keyword
; 匿名トークン: 一般的なキーワード。
[
  "abstract" "accessor" "as" "assert" "async" "await" "break"
  "case" "catch" "const" "continue" "debugger" "declare" "default"
  "delete" "do" "else" "enum" "export" "extends" "finally" "for" "from"
  "function" "get" "global" "if" "implements" "in" "infer"
  "instanceof" "interface" "is" "keyof" "let" "namespace" "new"
  "of" "override" "readonly" "private" "protected" "public" "return"
  "satisfies" "set" "static" "switch" "throw" "try" "typeof"
  "using" "var" "void" "while" "with" "yield"
] @keyword
; 名前付きトークン (匿名文字列パターンではマッチできない)。
(asserts) @keyword
(class) @keyword
(import) @keyword
(module) @keyword
(type) @keyword
(this) @keyword
(super) @keyword

; function (定義・呼び出し・メソッド・new)
(function_declaration name: (identifier) @function)
(function_expression name: (identifier) @function)
(generator_function name: (identifier) @function)
(generator_function_declaration name: (identifier) @function)
(function_signature name: (identifier) @function)
(method_definition name: (property_identifier) @function)
(method_signature name: (property_identifier) @function)
(call_expression function: (identifier) @function)
(call_expression function: (member_expression property: (property_identifier) @function))
(new_expression constructor: (identifier) @function)

; type
(predefined_type) @type
(type_identifier) @type

; parameter
(required_parameter pattern: (identifier) @parameter)
(optional_parameter pattern: (identifier) @parameter)

; field
(property_identifier) @field
(shorthand_property_identifier) @field
(private_property_identifier) @field

; operator
[
  "+" "-" "*" "/" "%" "=" "==" "!=" "<" ">" "<=" ">=" "&&" "||" "!" "&"
  "|" "^" "<<" ">>" "+=" "-=" "*=" "/=" "%=" "&=" "|=" "^=" "**" "**="
  "??" "?." "=>" "??=" "&&=" "||=" "++" "--" "..." "~"
] @operator

; punctuation (括弧・区切り)
[
  "(" ")" "[" "]" "{" "}" "," ";" ":"
] @punctuation

; attribute
(decorator) @attribute

; error (未構文)
(ERROR) @error