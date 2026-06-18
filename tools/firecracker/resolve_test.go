package main

import "testing"

func TestPickNearestAncestor(t *testing.T) {
	tests := []struct {
		name string
		in   []candidate
		want string // "" means nil
	}{
		{"empty", nil, ""},
		{"single", []candidate{{"a", 5}}, "a"},
		{"nearest wins", []candidate{{"far", 10}, {"near", 2}, {"mid", 5}}, "near"},
		{"tie broken by sha", []candidate{{"zzz", 3}, {"aaa", 3}}, "aaa"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := pickNearestAncestor(tt.in)
			if tt.want == "" {
				if got != nil {
					t.Fatalf("want nil, got %+v", got)
				}
				return
			}
			if got == nil || got.BaseSHA != tt.want {
				t.Fatalf("want %q, got %+v", tt.want, got)
			}
		})
	}
}

func TestSortCandidates(t *testing.T) {
	in := []candidate{{"b", 5}, {"a", 2}, {"c", 2}}
	got := sortCandidates(in)
	want := []string{"a", "c", "b"} // distance asc, then sha asc
	for i, c := range got {
		if c.BaseSHA != want[i] {
			t.Fatalf("at %d: want %q got %q (%+v)", i, want[i], c.BaseSHA, got)
		}
	}
	// input not mutated
	if in[0].BaseSHA != "b" {
		t.Fatalf("input was mutated: %+v", in)
	}
}
