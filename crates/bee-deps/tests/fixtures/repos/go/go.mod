module example.com/svc

go 1.22

require (
	github.com/gin-gonic/gin v1.7.2
	golang.org/x/text v0.3.6 // indirect
	github.com/local/lib v0.0.0
)

require gopkg.in/yaml.v3 v3.0.0-20210107192922-496545a6307b

replace github.com/local/lib => ../lib
