import com.google.devtools.build.lib.query2.proto.proto2api.Build;

interface BazelTarget {
    boolean hasRule();
    BazelRule getRule();
    boolean hasSourceFile();
    String getSourceFileName();
}

class BazelTargetImpl implements BazelTarget {
    private Build.Target target;

    public BazelTargetImpl(Build.Target target) {
        this.target = target;
    }

    @Override
    public boolean hasRule() {
        return target.hasRule();
    }

    @Override
    public BazelRule getRule() {
        if (this.hasRule()) {
            return new BazelRuleImpl(target.getRule());
        }
        return null;
    }

    @Override
    public boolean hasSourceFile() {
        return target.hasSourceFile();
    }

    @Override
    public String getSourceFileName() {
        if (this.hasSourceFile()) {
            return this.target.getSourceFile().getName();
        }
        return null;
    }
}

