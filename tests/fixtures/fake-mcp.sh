#!/usr/bin/env bash
set -u
printf '%s\n' "$*" > "${HOME}/fake-mcp-args"
page_ids="1"
page_urls="1=about:blank"
selected_page=1
next_page=2

json_escape() {
  printf '%s' "$1" | sed ':a;N;$!ba;s|\\|\\\\|g; s|"|\\"|g; s|\n|\\n|g'
}

field_string() {
  printf '%s' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"
}

field_number() {
  printf '%s' "$1" | sed -n "s/.*\"$2\":\([0-9][0-9]*\).*/\1/p"
}

field_bool() {
  printf '%s' "$1" | sed -n "s/.*\"$2\":\(true\|false\).*/\1/p"
}

field_array_string() {
  printf '%s' "$1" | sed -n "s/.*\"$2\":[[:space:]]*\[\"\([^\"]*\)\".*/\1/p"
}

page_url() {
  local id="$1"
  local entry
  for entry in $page_urls; do
    case "$entry" in
      "$id="*) printf '%s' "${entry#*=}"; return ;;
    esac
  done
  printf 'about:blank'
}

set_page_url() {
  local id="$1"
  local url="$2"
  local next=""
  local entry
  for entry in $page_urls; do
    case "$entry" in
      "$id="*) ;;
      *) next="${next}${next:+ }$entry" ;;
    esac
  done
  page_urls="${next}${next:+ }$id=$url"
}

remove_page() {
  local remove="$1"
  local next_ids=""
  local id
  for id in $page_ids; do
    [ "$id" = "$remove" ] && continue
    next_ids="${next_ids}${next_ids:+ }$id"
  done
  page_ids="$next_ids"
  local next_urls=""
  local entry
  for entry in $page_urls; do
    case "$entry" in
      "$remove="*) ;;
      *) next_urls="${next_urls}${next_urls:+ }$entry" ;;
    esac
  done
  page_urls="$next_urls"
  case " $page_ids " in
    *" $selected_page "*) ;;
    *) selected_page="${page_ids%% *}" ;;
  esac
}

pages_structured() {
  local first=1
  local id url title selected
  printf '['
  for id in $page_ids; do
    [ "$first" -eq 0 ] && printf ','
    first=0
    url=$(json_escape "$(page_url "$id")")
    title="Page $id"
    selected=false
    [ "$id" = "$selected_page" ] && selected=true
    printf '{"id":%s,"url":"%s","title":"%s","selected":%s}' "$id" "$url" "$title" "$selected"
  done
  printf ']'
}

pages_text() {
  local id url marker out="## Pages"
  for id in $page_ids; do
    url=$(page_url "$id")
    marker=""
    [ "$id" = "$selected_page" ] && marker=" [selected]"
    out="$out\n$id: Page $id ($url)$marker"
  done
  printf '%s' "$out"
}

tools_list() {
  cat <<JSON
[{"name":"click","inputSchema":{"type":"object","properties":{"pageId":{"type":"number"},"uid":{"type":"string"},"includeSnapshot":{"type":"boolean"}},"required":["pageId","uid"]}},{"name":"close_page","inputSchema":{"type":"object","properties":{"pageId":{"type":"number"}},"required":["pageId"]}},{"name":"drag","inputSchema":{"type":"object","properties":{"pageId":{"type":"number"},"from_uid":{"type":"string"},"to_uid":{"type":"string"}},"required":["pageId","from_uid","to_uid"]}},{"name":"evaluate_script","inputSchema":{"type":"object","properties":{"pageId":{"type":"number"},"function":{"type":"string"},"args":{"type":"array","items":{"type":"string"}}},"required":["pageId","function"]}},{"name":"fill_form","inputSchema":{"type":"object","properties":{"pageId":{"type":"number"},"elements":{"type":"array","items":{"type":"object","properties":{"uid":{"type":"string"}}}}},"required":["pageId","elements"]}},{"name":"list_pages","inputSchema":{"type":"object","properties":{},"required":[]}}, {"name":"new_page","inputSchema":{"type":"object","properties":{"url":{"type":"string"},"background":{"type":"boolean"}},"required":["url"]}},{"name":"select_page","inputSchema":{"type":"object","properties":{"pageId":{"type":"number"}},"required":["pageId"]}},{"name":"take_snapshot","inputSchema":{"type":"object","properties":{"pageId":{"type":"number"}},"required":["pageId"]}}]
JSON
}

tool_response() {
  local id="$1"
  local text="$2"
  local structured="${3:-{}}"
  text=$(json_escape "$text")
  printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}],"structuredContent":%s}\n' "$id" "$text" "$structured"
}

error_response() {
  local id="$1"
  local text="$2"
  text=$(json_escape "$text")
  printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}],"isError":true}}\n' "$id" "$text"
}

while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake-mcp","version":"0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":%s}}\n' "$id" "$(tools_list)" ;;
    *'"method":"tools/call"'*)
      name=$(field_string "$line" name)
      case "$name" in
        new_page)
          url=$(field_string "$line" url)
          [ -z "$url" ] && url="about:blank"
          page_id="$next_page"
          next_page=$((next_page + 1))
          page_ids="$page_ids $page_id"
          set_page_url "$page_id" "$url"
          selected_page="$page_id"
          tool_response "$id" "$(pages_text)" "{\"pages\":$(pages_structured)}" ;;
        list_pages)
          tool_response "$id" "$(pages_text)" "{\"pages\":$(pages_structured)}" ;;
        close_page)
          page_id=$(field_number "$line" pageId)
          count=$(printf '%s\n' $page_ids | wc -l)
          if [ "$count" -gt 1 ]; then
            remove_page "$page_id"
            tool_response "$id" "$(pages_text)" "{\"pages\":$(pages_structured)}"
          else
            error_response "$id" "The last open page cannot be closed. It is fine to keep it open."
          fi ;;
        select_page)
          page_id=$(field_number "$line" pageId)
          selected_page="$page_id"
          tool_response "$id" "$(pages_text)" "{\"pages\":$(pages_structured)}" ;;
        take_snapshot)
          page_id=$(field_number "$line" pageId)
          [ -z "$page_id" ] && page_id="$selected_page"
          tool_response "$id" "snapshot page=$page_id uid=${page_id}_button uid=${page_id}_after" "{}" ;;
        click)
          page_id=$(field_number "$line" pageId)
          [ -z "$page_id" ] && page_id="$selected_page"
          uid=$(field_string "$line" uid)
          if [ "$uid" = "${page_id}_button" ]; then
            sleep 0.5
            include_snapshot=$(field_bool "$line" includeSnapshot)
            if [ "$include_snapshot" = "true" ]; then
              tool_response "$id" "clicked page=$page_id raw=$uid\nsnapshot page=$page_id uid=${page_id}_after" "{}"
            else
              tool_response "$id" "clicked page=$page_id" "{}"
            fi
          else
            error_response "$id" "wrong page: page=$page_id uid=$uid selected=$selected_page"
          fi ;;
        drag)
          page_id=$(field_number "$line" pageId)
          [ -z "$page_id" ] && page_id="$selected_page"
          from_uid=$(field_string "$line" from_uid)
          to_uid=$(field_string "$line" to_uid)
          if [ "$from_uid" = "${page_id}_button" ] && [ "$to_uid" = "${page_id}_after" ]; then
            tool_response "$id" "dragged page=$page_id from=$from_uid to=$to_uid" "{}"
          else
            error_response "$id" "wrong drag page=$page_id from=$from_uid to=$to_uid"
          fi ;;
        evaluate_script)
          page_id=$(field_number "$line" pageId)
          [ -z "$page_id" ] && page_id="$selected_page"
          arg=$(field_array_string "$line" args)
          if [ -n "$arg" ] && [ "$arg" != "${page_id}_button" ]; then
            error_response "$id" "wrong eval arg page=$page_id arg=$arg"
          else
            tool_response "$id" "evaluated page=$page_id arg=$arg" "{}"
          fi ;;
        fill_form)
          page_id=$(field_number "$line" pageId)
          [ -z "$page_id" ] && page_id="$selected_page"
          uid=$(field_string "$line" uid)
          if [ "$uid" = "${page_id}_button" ]; then
            tool_response "$id" "filled form page=$page_id raw=$uid" "{}"
          else
            error_response "$id" "wrong form page=$page_id uid=$uid"
          fi ;;
        navigate_page)
          page_id=$(field_number "$line" pageId)
          [ -z "$page_id" ] && page_id="$selected_page"
          url=$(field_string "$line" url)
          [ -n "$url" ] && set_page_url "$page_id" "$url"
          tool_response "$id" "navigated page=$page_id" "{}" ;;
        *)
          page_id=$(field_number "$line" pageId)
          [ -z "$page_id" ] && page_id="$selected_page"
          tool_response "$id" "ok page=$page_id" "{}" ;;
      esac ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
