package main

import "sort"

// candidate is a stored snapshot base SHA that is an ancestor of the target SHA,
// together with how many commits separate it from the target.
type candidate struct {
	BaseSHA  string
	Distance int // git rev-list --count <baseSHA>..<targetSHA>
}

// pickNearestAncestor chooses the snapshot whose base SHA is the *nearest*
// ancestor of the target (fewest commits between base and target), so the
// warm server has to re-analyse the least. Ties are broken by lexically
// smallest SHA so the choice is deterministic.
//
// Returns nil when there are no candidates, which the caller treats as a
// cold-fallback signal.
func pickNearestAncestor(candidates []candidate) *candidate {
	if len(candidates) == 0 {
		return nil
	}
	best := candidates[0]
	for _, c := range candidates[1:] {
		if c.Distance < best.Distance ||
			(c.Distance == best.Distance && c.BaseSHA < best.BaseSHA) {
			best = c
		}
	}
	return &best
}

// sortCandidates returns candidates ordered nearest-first (for display/logging).
func sortCandidates(candidates []candidate) []candidate {
	out := append([]candidate(nil), candidates...)
	sort.Slice(out, func(i, j int) bool {
		if out[i].Distance != out[j].Distance {
			return out[i].Distance < out[j].Distance
		}
		return out[i].BaseSHA < out[j].BaseSHA
	})
	return out
}
