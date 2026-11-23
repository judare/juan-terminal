; Curated for tree-sitter-highlight (no nvim-only predicates).
; Based on tree-sitter-vue-next queries (MIT, see LICENSE).

; <script> without lang → javascript
((script_element
  (start_tag . (tag_name) .)
  (raw_text) @injection.content)
  (#set! injection.language "javascript"))

; <script lang="ts|js|tsx|jsx">
((script_element
  (start_tag
    (attribute
      (attribute_name) @_lang
      (quoted_attribute_value (attribute_value) @injection.language)))
  (raw_text) @injection.content)
  (#eq? @_lang "lang"))

; <style> without lang → css
((style_element
  (start_tag . (tag_name) .)
  (raw_text) @injection.content)
  (#set! injection.language "css"))

; <style lang="scss|css"> (and <style scoped lang=...>)
((style_element
  (start_tag
    (attribute
      (attribute_name) @_lang
      (quoted_attribute_value (attribute_value) @injection.language)))
  (raw_text) @injection.content)
  (#eq? @_lang "lang"))

; <style scoped> → css
((style_element
  (start_tag
    (attribute (attribute_name) @_scoped))
  (raw_text) @injection.content)
  (#eq? @_scoped "scoped")
  (#set! injection.language "css"))

; {{ expression }}
((interpolation
  (raw_text) @injection.content)
  (#set! injection.language "typescript"))

; v-if="..." :prop="..." @click="..."
(directive_attribute
  (quoted_attribute_value
    (attribute_value) @injection.content)
  (#set! injection.language "typescript"))
