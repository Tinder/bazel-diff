package lib

import "testing"

func TestErr(t *testing.T) {
	if Err() == nil {
		t.Fail()
	}
}
