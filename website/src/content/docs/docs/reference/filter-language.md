---
title: Filter language
description: The grammar, fields, operators, values and functions of the filter bar, with examples.
---

Use this page to write filters for the filter bar, the REST API and the dashboard. For a walk-through, see [Filters and search](/docs/tracing/filters-and-search/).

A filter is a SQL boolean expression: the text you would write after `WHERE`. cetra checks it against one view at a time: `traces`, `spans`, `sessions` or `scores`, depending on the page. The columns of each view are in the [SQL schema](/docs/reference/sql-schema/).

## Grammar

```text
filter     = expression
expression = expression ("AND" | "OR") expression
           | "NOT" expression
           | "(" expression ")"
           | comparison
comparison = operand operator operand
           | operand "IS" ["NOT"] "NULL"
           | operand "IS" ["NOT"] "DISTINCT" "FROM" operand
           | operand ["NOT"] "IN" "(" value {"," value} ")"
           | operand ["NOT"] "BETWEEN" value "AND" value
           | operand ["NOT"] ("LIKE" | "ILIKE") string
operand    = column | map-access | function-call | value | operand arithmetic operand
map-access = ("metadata" | "attributes" | "resource_attributes") "[" string "]"
value      = string | number | "TRUE" | "FALSE" | "NULL" | interval | list
interval   = "INTERVAL" string
list       = "[" [value {"," value}] "]"
```

- Column names are the columns of the view. They are case-insensitive unless you put them in double quotes. You cannot qualify them with a table name.
- Anything outside this grammar, such as a subquery, a window function, an unknown column or an unknown function, is refused with the position of the problem.
- A filter has at most 4 KiB of text and 128 operators.

## Fields

Every column of the view is a field. Which view applies depends on where you type the filter:

| Where | View |
|---|---|
| The traces list, the dashboard | `traces` |
| The spans list | `spans` |
| The sessions list | `sessions` |
| The scores in the API | `scores` |

`metadata`, `attributes` and `resource_attributes` are maps. Read one key with `metadata['customer']`. A key that is not present is null.

`duration_ms` is `duration_ns` divided by one million. It is available on `spans`, `traces` and `sessions`.

## Operators

| Column type | Operators |
|---|---|
| `string` | `=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`, `IN`, `NOT IN`, `LIKE`, `ILIKE`, `IS NULL`, `IS NOT NULL` |
| `integer`, `float` | `=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN`, arithmetic (`+ - * / %`), `IS NULL` |
| `boolean` | `=`, `!=`, `NOT`, or the column alone (`has_root`) |
| `timestamp` | `=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IS NULL`, and arithmetic with an interval |
| `string_list` | `@>` (contains all of a list), `array_has`, `array_length` |
| `string_map` | `[key]` access, then the operators of a string |

`AND`, `OR` and `NOT` combine comparisons. `AND` binds tighter than `OR`: use parentheses when you mix them. `IS DISTINCT FROM` compares like `<>` but treats null as a value.

`LIKE` is case-sensitive and `ILIKE` is not. In both, `%` matches any run of characters and `_` matches one character.

You can convert a value with `CAST(x AS type)` or `x::type` to `BIGINT`, `DOUBLE`, `VARCHAR`, `BOOLEAN` or `TIMESTAMP`. A value that cannot be converted becomes null.

## Values

| Kind | Syntax | Example |
|---|---|---|
| String | Single quotes. Double a quote to write one. | `'error'`, `'it''s'` |
| Number | Digits with an optional fraction and exponent. | `42`, `0.10`, `5e9` |
| Boolean | `TRUE` or `FALSE` | `TRUE` |
| Null | `NULL` | `NULL` |
| Duration | A number in the unit of the column. Durations are in nanoseconds (`duration_ns`) or milliseconds (`duration_ms`). | `duration_ns > 5e9`, `duration_ms > 5000` |
| Time | An RFC 3339 string, or `now()` minus or plus an interval. | `'2026-01-01T10:05:00Z'`, `now() - INTERVAL '1 hour'` |
| List | Square brackets. | `['beta', 'vip']` |

## Functions

| Function | Use |
|---|---|
| `now()` | The current time. |
| `lower(s)`, `upper(s)`, `length(s)` | Change case, or count characters. |
| `coalesce(a, b)` | The first value that is not null. |
| `array_has(list, value)`, `array_length(list)` | Look into a list column such as `tags` or `models`. |
| `date_trunc(unit, time)` | Round a time down to a unit such as `hour` or `day`. |
| `abs(x)` | The absolute value. |
| `score(name)` | The latest numeric value of the score `name`, on `traces`, `spans` and `sessions`. |
| `score_label(name)` | The latest label or text of the score `name`. |
| `has_score(name)` | Whether a score of that name exists. |

A function takes plain arguments: no `DISTINCT`, `FILTER`, `OVER`, `ORDER BY` or named arguments. The score functions take a string literal. One filter can use at most 8 different score names.

## Full-text search

The search box is separate from the filter. It looks for the text anywhere in the `input`, `output`, `name` and `system_instructions` of spans, case-insensitively. cetra escapes `%`, `_` and `\` for you, so you type the text as it is. The text is at most 1,024 bytes. Search needs a time range. On the traces list, a trace matches when any of its spans does.

To search a single column with your own pattern, use `ILIKE` in the filter.

## Examples

<!-- examples checked by hand on 2026-11-17 -->

| Filter | Finds |
|---|---|
| `status = 'error' AND model LIKE 'gpt-5%'` | On the spans list: failed calls to a model whose name starts with `gpt-5`. |
| `cost_total > 0.10 AND tags @> ['beta']` | On the traces list: traces that cost more than ten cents and carry the tag `beta`. |
| `metadata['customer'] = 'acme' AND duration_ns > 5e9` | On the traces list: traces for one customer that took longer than five seconds. |
| `start_time > now() - INTERVAL '1 hour'` | Rows that started in the last hour. |
| `score('hallucination') >= 0.5` | On the traces list: traces whose latest `hallucination` score is 0.5 or more. |
| `input ILIKE '%refund%'` | On the spans list: spans whose input mentions a refund, in any case. |
| `span_kind = 'tool' AND status = 'error'` | On the spans list: tool calls that failed. |
| `environment IN ('production', 'staging') AND release = '1.4.2'` | On the traces list: traces from one release in two environments. |
| `user_id IS NULL` | On the traces list: traces with no user. |
| `duration_ms BETWEEN 1000 AND 5000` | On the traces list: traces that took between one and five seconds. |
| `array_has(models, 'gpt-5')` | On the traces list: traces that called the model `gpt-5`. |
| `has_score('user_feedback') AND score_label('sentiment') = 'negative'` | On the traces list: traces with user feedback and a negative sentiment label. |
| `trace_count > 10 AND cost_total > 1` | On the sessions list: sessions of more than ten traces that cost more than one dollar. |
