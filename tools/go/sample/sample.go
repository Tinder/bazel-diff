// Package sample is a placeholder that exercises the Go coverage gate
// end-to-end. Replace it with real tooling code -- the >=90% line-coverage
// requirement enforced in CI applies to everything under tools/go/.
package sample

// Add returns the sum of a and b.
func Add(a, b int) int {
	return a + b
}

// Max returns the larger of a and b.
func Max(a, b int) int {
	if a > b {
		return a
	}
	return b
}
