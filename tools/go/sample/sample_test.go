package sample

import "testing"

func TestAdd(t *testing.T) {
	if got := Add(2, 3); got != 5 {
		t.Fatalf("Add(2, 3) = %d, want 5", got)
	}
}

func TestMax(t *testing.T) {
	if got := Max(2, 3); got != 3 {
		t.Fatalf("Max(2, 3) = %d, want 3", got)
	}
	if got := Max(5, 1); got != 5 {
		t.Fatalf("Max(5, 1) = %d, want 5", got)
	}
}
