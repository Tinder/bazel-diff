package lib

import "github.com/pkg/errors"

// Err returns a sentinel error so that //:lib has a real reason to depend on
// github.com/pkg/errors.
func Err() error { return errors.New("err") }
