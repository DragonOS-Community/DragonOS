# Minimal makefile for Sphinx documentation
#

# You can set these variables from the command line, and also
# from the environment for the first two.
SPHINXOPTS    ?=
SPHINXBUILD   ?= sphinx-build
SOURCEDIR     = .
BUILDDIR      = _build

LANGUAGE      = zh_CN

SPHINXOPTS	+= -D language=$(LANGUAGE)

export CURRENT_GIT_COMMIT_HASH := $(shell git rev-parse --short HEAD)

GIT_STATUS := $(shell git status --porcelain --untracked-files=no)

ifeq ($(strip $(GIT_STATUS)),)
    export CURRENT_GIT_COMMIT_DIRTY = 0
else
    export CURRENT_GIT_COMMIT_DIRTY = 1
endif

# Put it first so that "make" without argument is like "make help".
help:
	@$(SPHINXBUILD) -M help "$(SOURCEDIR)" "$(BUILDDIR)" $(SPHINXOPTS) $(O)

.PHONY: help Makefile

# Catch-all target: route all unknown targets to Sphinx using the new
# "make mode" option.  $(O) is meant as a shortcut for $(SPHINXOPTS).
%: Makefile
	@$(SPHINXBUILD) -M $@ "$(SOURCEDIR)" "$(BUILDDIR)" $(SPHINXOPTS) $(O)

.PHONY:
html-multiversion:
	rm -rf ./$(BUILDDIR) && CURRENT_GIT_COMMIT_DIRTY=0 sphinx-multiversion $(SPHINXOPTS) "$(SOURCEDIR)" ./$(BUILDDIR)/html && cp -rf ./$(BUILDDIR)/html/master/* ./$(BUILDDIR)/html/
