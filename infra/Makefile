TF := tofu

bootstrap:
	cd bootstrap && $(TF) init && $(TF) apply

inception:
	cd inception && $(TF) init && $(TF) apply

platform:
	cd platform && $(TF) init && $(TF) apply

fmt:
	$(TF) fmt -recursive

.PHONY: bootstrap inception platform fmt
