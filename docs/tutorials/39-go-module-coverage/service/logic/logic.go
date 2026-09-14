package logic

func Classify(value int) string {
	if value == 7 {
		return "seven"
	}
	if value%2 == 0 {
		return "even"
	}
	return "odd"
}
