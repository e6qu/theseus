package main

import (
	"fmt"
	"os"
	"strconv"

	"example.com/classifier/logic"
)

func main() {
	value, err := strconv.Atoi(os.Args[1])
	if err != nil {
		panic(err)
	}
	fmt.Printf("class=%s\n", logic.Classify(value))
}
