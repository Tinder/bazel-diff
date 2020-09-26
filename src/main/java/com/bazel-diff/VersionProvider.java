import picocli.CommandLine.IVersionProvider;

class VersionProvider implements IVersionProvider {
    public String[] getVersion() throws Exception {
        return new String[] {
            "1.0.2"
        };
    }
}
