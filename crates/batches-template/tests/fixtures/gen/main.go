// Generates ../go_text_template.json: the output (or error) of Go's text/template for each
// case in ../cases.json, set up like bulk change spec templates: delimiters ${{ }}, the bulk change spec
// functions (below) and the case's top-level keys
// exposed as niladic functions, with the data as the dot.
//
// Run (from the repository root):
//
//	docker run --rm -v "$PWD/crates/batches-template/tests/fixtures:/f" -w /f/gen golang:1.23-alpine \
//	  sh -c 'go mod tidy >/dev/null 2>&1; go run . ../cases.json ../go_text_template.json'
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"text/template"

	"github.com/gobwas/glob"
)

// builtins are the bulk change spec template functions.
var builtins = template.FuncMap{
	"join": func(elems []string, sep string) string {
		return strings.Join(elems, sep)
	},
	"split": func(s, sep string) []string {
		return strings.Split(s, sep)
	},
	"replace": func(s, old, new string) string {
		return strings.ReplaceAll(s, old, new)
	},
	"join_if": func(sep string, elems ...string) string {
		kept := make([]string, 0, len(elems))
		for _, e := range elems {
			if e != "" {
				kept = append(kept, e)
			}
		}
		return strings.Join(kept, sep)
	},
	"matches": func(in, pattern string) (bool, error) {
		g, err := glob.Compile(pattern)
		if err != nil {
			return false, err
		}
		return g.Match(in), nil
	},
}

type testCase struct {
	Name       string          `json:"name"`
	Template   string          `json:"template"`
	Data       json.RawMessage `json:"data,omitempty"`
	MissingKey string          `json:"missingkey,omitempty"`
}

type result struct {
	Name       string          `json:"name"`
	Template   string          `json:"template"`
	Data       json.RawMessage `json:"data,omitempty"`
	MissingKey string          `json:"missingkey,omitempty"`
	Output     *string         `json:"output,omitempty"`
	Error      *string         `json:"error,omitempty"`
}

// convert turns JSON numbers into int when integral (like yaml.v3), float64 otherwise.
func convert(v any) any {
	switch x := v.(type) {
	case json.Number:
		if i, err := x.Int64(); err == nil {
			return int(i)
		}
		f, _ := x.Float64()
		return f
	case []any:
		// Lists of strings become []string, as the changed-file lists and search result
		// paths are in bulk change spec template contexts.
		strs := make([]string, 0, len(x))
		for i := range x {
			x[i] = convert(x[i])
			if s, ok := x[i].(string); ok {
				strs = append(strs, s)
			}
		}
		if len(strs) == len(x) && len(x) > 0 {
			return strs
		}
		return x
	case map[string]any:
		for k := range x {
			x[k] = convert(x[k])
		}
		return x
	}
	return v
}

func run(c testCase) (out string, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic: %v", r)
		}
	}()
	var data map[string]any
	if len(c.Data) > 0 {
		d := json.NewDecoder(bytes.NewReader(c.Data))
		d.UseNumber()
		var raw any
		if err := d.Decode(&raw); err != nil {
			return "", err
		}
		data, _ = convert(raw).(map[string]any)
	}
	funcs := template.FuncMap{}
	for k, v := range data {
		v := v
		funcs[k] = func() any { return v }
	}
	mk := c.MissingKey
	if mk == "" {
		mk = "error"
	}
	t, err := template.New("t").Delims("${{", "}}").Option("missingkey=" + mk).Funcs(builtins).Funcs(funcs).Parse(c.Template)
	if err != nil {
		return "", err
	}
	var buf bytes.Buffer
	if err := t.Execute(&buf, data); err != nil {
		return "", err
	}
	return buf.String(), nil
}

func main() {
	in, err := os.ReadFile(os.Args[1])
	if err != nil {
		panic(err)
	}
	var cases []testCase
	if err := json.Unmarshal(in, &cases); err != nil {
		panic(err)
	}
	var results []result
	for _, c := range cases {
		r := result{Name: c.Name, Template: c.Template, Data: c.Data, MissingKey: c.MissingKey}
		out, err := run(c)
		if err != nil {
			s := err.Error()
			r.Error = &s
		} else {
			r.Output = &out
		}
		results = append(results, r)
	}
	b, err := json.MarshalIndent(results, "", "  ")
	if err != nil {
		panic(err)
	}
	b = append(b, '\n')
	if err := os.WriteFile(os.Args[2], b, 0o644); err != nil {
		panic(err)
	}
}
